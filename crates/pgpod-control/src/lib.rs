//! pgpod's control plane, and the library facade the CLI drives.
//!
//! Everything `pgpod` the command can do goes through [`Pgpod`], so an
//! embedder has exactly the same surface (`AGENTS.md` principle 7).

mod names;
mod password;
mod status;

use std::time::Duration;

use pgpod_core::{
    Bootstrap, ClusterId, ClusterManifest, InstanceId, InstancePhase, InstanceRole, InstanceSpec,
    PathLayout, SPEC_ENV, Secret, container,
};
use pgpod_pg::RESERVED_PARAMETERS;
use pgpod_registry::{InstanceRecord, Registry};
use pgpod_runtime::{
    ContainerSpec, ExecSpec, Mount, PodmanClient, PortPublish, SecretMount, VolumeInfo,
};

pub use names::{LABEL_CLUSTER, LABEL_INSTANCE, SecretNames};
pub use status::{ClusterStatus, InstanceStatus};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Podman(#[from] pgpod_runtime::Error),

    #[error(transparent)]
    Registry(#[from] pgpod_registry::Error),

    #[error(transparent)]
    Postgres(#[from] pgpod_pg::Error),

    #[error(transparent)]
    Manifest(#[from] pgpod_core::ManifestError),

    #[error(transparent)]
    Spec(#[from] pgpod_core::SpecError),

    #[error(
        "the pgpod agent is not installed at {0} — build and install it with \
         ops/build-agent.sh"
    )]
    AgentMissing(String),

    #[error("could not allocate a host port: {0}")]
    PortAllocation(String),

    #[error("instance {instance} did not become ready within {seconds}s\n{detail}")]
    NotReady {
        instance: String,
        seconds: u64,
        detail: String,
    },

    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The pgpod control plane.
pub struct Pgpod {
    podman: PodmanClient,
    registry: Registry,
    layout: PathLayout,
}

/// What `apply` did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApplyReport {
    pub cluster: String,
    pub generation: i64,
    pub created: bool,
    pub instances: Vec<InstanceSummary>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InstanceSummary {
    pub instance: String,
    pub container: String,
    pub volume: String,
    pub host_port: u16,
    pub connection_uri: String,
}

/// What `delete` did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeleteReport {
    pub cluster: String,
    pub containers_removed: Vec<String>,
    /// Empty unless `--purge` was given. Populated means databases were
    /// destroyed.
    pub volumes_removed: Vec<String>,
    /// Kept volumes, so the operator can see their data is still there.
    pub volumes_retained: Vec<String>,
}

impl Pgpod {
    pub fn new(podman: PodmanClient, registry: Registry, layout: PathLayout) -> Self {
        Self {
            podman,
            registry,
            layout,
        }
    }

    /// Connect to podman and open the registry using the XDG layout.
    pub fn open() -> Result<Self> {
        let layout = PathLayout::from_env();
        let podman = PodmanClient::connect()?;
        let registry = Registry::open(&layout.registry_db())?;
        Ok(Self::new(podman, registry, layout))
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    // ---- apply -------------------------------------------------------

    /// Create or converge a cluster.
    pub async fn apply(&self, manifest: &ClusterManifest, wait: Duration) -> Result<ApplyReport> {
        let cluster = manifest.cluster_id()?;
        self.validate_parameters(manifest)?;

        let agent = self.layout.agent_binary_for_host();
        if !agent.exists() {
            return Err(Error::AgentMissing(agent.display().to_string()));
        }

        let existed = self.registry.cluster(&cluster)?.is_some();
        let generation = self.registry.put_cluster(manifest, "applying")?;
        self.registry
            .record_event(cluster.as_str(), None, "info", "apply requested")?;

        // One network per cluster even at a single instance, so adding a
        // standby later does not have to move a running primary onto a
        // different network.
        self.podman
            .ensure_network(&cluster.network_name(), &names::cluster_labels(&cluster))
            .await?;

        let secrets = self.ensure_secrets(&cluster, manifest).await?;

        let mut summaries = Vec::new();
        for ordinal in 1..=manifest.spec.instances {
            let instance = cluster.instance(ordinal);
            let summary = self
                .apply_instance(manifest, &instance, &secrets, &agent)
                .await?;
            summaries.push(summary);
        }

        if !wait.is_zero() {
            for ordinal in 1..=manifest.spec.instances {
                self.wait_ready(&cluster.instance(ordinal), wait).await?;
            }
        }

        self.registry.set_cluster_phase(&cluster, "running")?;
        self.registry
            .record_event(cluster.as_str(), None, "info", "apply complete")?;

        Ok(ApplyReport {
            cluster: cluster.to_string(),
            generation,
            created: !existed,
            instances: summaries,
        })
    }

    /// Reject reserved parameters before anything is created.
    ///
    /// The renderer would catch these too, but only once the agent is
    /// already running inside a container — leaving a `Failed` instance
    /// and a volume behind for a mistake that is visible in the manifest.
    fn validate_parameters(&self, manifest: &ClusterManifest) -> Result<()> {
        for (key, _) in manifest.parameters() {
            let normalized = key.trim().to_ascii_lowercase();
            if RESERVED_PARAMETERS.contains(&normalized.as_str()) {
                return Err(Error::Postgres(pgpod_pg::Error::ReservedParameter(
                    normalized,
                )));
            }
        }
        Ok(())
    }

    async fn apply_instance(
        &self,
        manifest: &ClusterManifest,
        instance: &InstanceId,
        secrets: &SecretNames,
        agent: &std::path::Path,
    ) -> Result<InstanceSummary> {
        let cluster = instance.cluster().clone();
        let existing = self.registry.instance(&cluster, instance.ordinal())?;

        // Reuse the port a previous apply chose, so applications already
        // pointed at it keep working across a recreate.
        let host_port = match &existing {
            Some(rec) => rec.host_port,
            None => allocate_port()?,
        };

        let volume: VolumeInfo = self
            .podman
            .create_volume(&instance.volume_name(), &names::instance_labels(instance))
            .await?;

        self.registry.put_instance(&InstanceRecord {
            cluster: cluster.to_string(),
            ordinal: instance.ordinal(),
            container_name: instance.container_name(),
            container_id: None,
            volume_name: volume.name.clone(),
            host_port,
            phase: InstancePhase::Creating,
            role: InstanceRole::Unknown,
            timeline: None,
            last_probe_at_ms: None,
        })?;

        // Adopt a container that already exists rather than recreating it
        // — recreating a running instance would be a pointless outage.
        let handle = self.podman.container(instance.container_name());
        let probe = handle.probe().await?;
        let container_id = match probe {
            Some(p) if p.running => p.id,
            Some(p) => {
                self.podman.container(&p.id).start().await?;
                p.id
            }
            None => {
                let spec = self.container_spec(manifest, instance, host_port, secrets, agent)?;
                self.podman
                    .pull_image_if_absent(&manifest.spec.image_name)
                    .await?;
                let c = self.podman.create_container(&spec).await?;
                c.start().await?;
                c.id().to_string()
            }
        };

        self.registry.put_instance(&InstanceRecord {
            cluster: cluster.to_string(),
            ordinal: instance.ordinal(),
            container_name: instance.container_name(),
            container_id: Some(container_id),
            volume_name: volume.name.clone(),
            host_port,
            phase: InstancePhase::Bootstrapping,
            role: InstanceRole::Unknown,
            timeline: None,
            last_probe_at_ms: None,
        })?;

        Ok(InstanceSummary {
            instance: instance.to_string(),
            container: instance.container_name(),
            volume: volume.name,
            host_port,
            connection_uri: connection_uri(host_port, manifest),
        })
    }

    fn container_spec(
        &self,
        manifest: &ClusterManifest,
        instance: &InstanceId,
        host_port: u16,
        secrets: &SecretNames,
        agent: &std::path::Path,
    ) -> Result<ContainerSpec> {
        let spec = InstanceSpec {
            instance: instance.clone(),
            port: 5432,
            bootstrap: Bootstrap::Initdb(manifest.spec.bootstrap.initdb.clone()),
            parameters: manifest.parameters(),
            shared_preload_libraries: manifest.spec.postgresql.shared_preload_libraries.clone(),
            // Single-instance clusters need no pg_hba network rules, and a
            // guessed CIDR would be either useless or too wide.
            network_cidr: None,
            // Phase 2 turns this on. Until then archive_mode renders off
            // rather than archiving to nowhere.
            archive_command: None,
        };

        let uid = manifest.spec.postgres_uid;
        let gid = manifest.spec.postgres_gid;

        let mut c = ContainerSpec::hardened(&manifest.spec.image_name)
            .name(instance.container_name())
            .hostname(instance.container_name())
            // Explicit, because the official images declare no USER and
            // postgres refuses to run as root — see
            // container::DEFAULT_POSTGRES_UID.
            .user(format!("{uid}:{gid}"))
            .entrypoint([container::AGENT_BIN, "instance", "run"])
            .env(SPEC_ENV, spec.to_env_value()?)
            .network(instance.cluster().network_name())
            // Loopback only: a database should not land on the LAN
            // because someone forgot a flag.
            .publish(PortPublish::loopback(host_port, 5432))
            .mount(Mount::Volume {
                name: instance.volume_name(),
                target: container::VOLUME_MOUNT.to_string(),
                chown: false,
            })
            .mount(Mount::BindReadOnly {
                source: agent.display().to_string(),
                target: container::AGENT_BIN.to_string(),
            });

        for (name, target) in [
            (&secrets.superuser, container::SECRET_SUPERUSER),
            (&secrets.replication, container::SECRET_REPLICATION),
            (&secrets.monitor, container::SECRET_MONITOR),
            (&secrets.app_owner, container::SECRET_APP_OWNER),
        ] {
            c = c.secret(SecretMount {
                name: name.clone(),
                target: target.to_string(),
                mode: 0o400,
                uid,
                gid,
            });
        }

        for (k, v) in names::instance_labels(instance) {
            c = c.label(k, v);
        }
        Ok(c)
    }

    /// Create the cluster's secrets if they do not exist.
    ///
    /// Existing secrets are left alone. Regenerating them on every apply
    /// would rotate the superuser password out from under a running
    /// instance, and worse, out from under any standby using it to stream.
    async fn ensure_secrets(
        &self,
        cluster: &ClusterId,
        manifest: &ClusterManifest,
    ) -> Result<SecretNames> {
        let names = SecretNames::for_cluster(cluster);
        let wants_app = manifest.spec.bootstrap.initdb.database.is_some();

        for name in names.all() {
            if name == names.app_owner && !wants_app {
                continue;
            }
            if !self.podman.secret_exists(name).await? {
                let generated: Secret = password::generate();
                self.podman.put_secret(name, generated.expose()).await?;
            }
        }
        Ok(names)
    }

    async fn wait_ready(&self, instance: &InstanceId, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let container = self.podman.container(instance.container_name());
        let mut last = String::new();

        while std::time::Instant::now() < deadline {
            // pg_isready rather than the agent's status socket: it is the
            // narrowest check available and does not depend on pgpod's own
            // code being correct.
            let probe =
                ExecSpec::new(["pg_isready", "-h", container::SOCKET_DIR, "-U", "postgres"]);
            match container.exec(&probe).await {
                Ok(out) if out.success() => {
                    let cluster = instance.cluster().clone();
                    if let Some(mut rec) = self.registry.instance(&cluster, instance.ordinal())? {
                        rec.phase = InstancePhase::Running;
                        self.registry.put_instance(&rec)?;
                    }
                    return Ok(());
                }
                Ok(out) => last = format!("{}{}", out.stdout.trim(), out.stderr.trim()),
                Err(e) => last = e.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Container logs are where the real reason lives — a failed
        // initdb, a bad parameter, a missing secret.
        let logs = container
            .logs_string()
            .await
            .unwrap_or_else(|e| format!("<logs unavailable: {e}>"));
        Err(Error::NotReady {
            instance: instance.to_string(),
            seconds: timeout.as_secs(),
            detail: format!("last probe: {last}\n\ncontainer logs:\n{logs}"),
        })
    }

    // ---- status ------------------------------------------------------

    pub async fn status(&self, cluster: &ClusterId) -> Result<ClusterStatus> {
        status::collect(self, cluster).await
    }

    pub async fn list(&self) -> Result<Vec<ClusterStatus>> {
        let mut out = Vec::new();
        for record in self.registry.list_clusters()? {
            let id = ClusterId::new(record.name.clone())
                .map_err(|e| Error::Invalid(format!("stored cluster name: {e}")))?;
            out.push(self.status(&id).await?);
        }
        Ok(out)
    }

    pub(crate) fn podman(&self) -> &PodmanClient {
        &self.podman
    }

    // ---- delete ------------------------------------------------------

    /// Remove a cluster's containers, and its volumes only if `purge`.
    ///
    /// Without `purge` the volumes stay and a later `apply` adopts them
    /// with the data intact. This is the difference between a stop and a
    /// deletion, and it is deliberately the default (`AGENTS.md`
    /// principle 4).
    pub async fn delete(&self, cluster: &ClusterId, purge: bool) -> Result<DeleteReport> {
        let record = self.registry.require_cluster(cluster)?;
        let instances = self.registry.instances(cluster)?;

        let mut containers_removed = Vec::new();
        let mut volumes_removed = Vec::new();
        let mut volumes_retained = Vec::new();

        for inst in &instances {
            let handle = self.podman.container(&inst.container_name);
            if handle.probe().await?.is_some() {
                let _ = handle.stop(Duration::from_secs(30)).await;
                handle.remove(true).await?;
                containers_removed.push(inst.container_name.clone());
            }
        }

        for inst in &instances {
            if purge {
                self.podman
                    .remove_volume_destroying_data(&inst.volume_name)
                    .await?;
                volumes_removed.push(inst.volume_name.clone());
            } else {
                volumes_retained.push(inst.volume_name.clone());
            }
        }

        // The network holds no data and podman refuses to remove one that
        // still has containers attached, so this is safe either way.
        let _ = self.podman.remove_network(&cluster.network_name()).await;

        if purge {
            for name in SecretNames::for_cluster(cluster).all() {
                let _ = self.podman.remove_secret(name).await;
            }
            self.registry.delete_cluster(cluster)?;
        } else {
            // Keep the rows: they are what lets a later apply find the
            // retained volumes and reuse the same ports.
            self.registry.set_cluster_phase(cluster, "stopped")?;
        }

        self.registry.record_event(
            &record.name,
            None,
            "info",
            if purge {
                "cluster deleted and volumes purged"
            } else {
                "cluster deleted, volumes retained"
            },
        )?;

        Ok(DeleteReport {
            cluster: cluster.to_string(),
            containers_removed,
            volumes_removed,
            volumes_retained,
        })
    }
}

/// A libpq URI for an instance.
pub fn connection_uri(host_port: u16, manifest: &ClusterManifest) -> String {
    let (user, database) = match (
        &manifest.spec.bootstrap.initdb.owner,
        &manifest.spec.bootstrap.initdb.database,
    ) {
        (Some(owner), Some(db)) => (owner.clone(), db.clone()),
        _ => ("postgres".to_string(), "postgres".to_string()),
    };
    format!("postgresql://{user}@127.0.0.1:{host_port}/{database}")
}

/// Ask the kernel for a free port.
///
/// Inherently racy — something else can take it between here and the
/// container starting — but the alternative is a fixed range that collides
/// with whatever else the host runs. The port is persisted on first
/// allocation so a recreate keeps it.
fn allocate_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| Error::PortAllocation(e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::PortAllocation(e.to_string()))?
        .port();
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(yaml: &str) -> ClusterManifest {
        ClusterManifest::from_yaml(yaml).unwrap()
    }

    #[test]
    fn connection_uri_uses_the_app_role_when_there_is_one() {
        let m = manifest(
            "apiVersion: pgpod/v1\nkind: Cluster\nmetadata:\n  name: mydb\nspec:\n  \
             imageName: postgres:18\n  bootstrap:\n    initdb:\n      database: appdb\n      owner: app\n",
        );
        assert_eq!(
            connection_uri(15432, &m),
            "postgresql://app@127.0.0.1:15432/appdb"
        );
    }

    #[test]
    fn connection_uri_falls_back_to_postgres_without_an_app_database() {
        let m = manifest(
            "apiVersion: pgpod/v1\nkind: Cluster\nmetadata:\n  name: mydb\nspec:\n  imageName: postgres:18\n",
        );
        assert_eq!(
            connection_uri(15432, &m),
            "postgresql://postgres@127.0.0.1:15432/postgres"
        );
    }

    #[test]
    fn allocated_ports_are_usable_and_not_privileged() {
        let p = allocate_port().unwrap();
        assert!(p >= 1024, "must not need privileges: {p}");
    }

    #[test]
    fn a_uri_never_contains_a_password() {
        // Passwords live in podman secrets; a URI is printed to terminals
        // and pasted into tickets.
        let m = manifest(
            "apiVersion: pgpod/v1\nkind: Cluster\nmetadata:\n  name: mydb\nspec:\n  \
             imageName: postgres:18\n  bootstrap:\n    initdb:\n      database: appdb\n      owner: app\n",
        );
        let uri = connection_uri(15432, &m);
        assert!(!uri.contains(':') || !uri.contains('@') || uri.matches(':').count() <= 2);
        assert!(!uri.to_lowercase().contains("password"));
    }
}
