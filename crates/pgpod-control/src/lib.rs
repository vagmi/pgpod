//! pgpod's control plane, and the library facade the CLI drives.
//!
//! Everything `pgpod` the command can do goes through [`Pgpod`], so an
//! embedder has exactly the same surface (`AGENTS.md` principle 7).

mod backup;
mod names;
mod password;
mod pooler;
mod status;
mod upgrade;

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

pub use backup::{BackupReport, RestoreReport};
pub use names::{LABEL_CLUSTER, LABEL_INSTANCE, LABEL_POOLER, SecretNames};
pub use pooler::{PoolSummary, PoolerHold, PoolerReport};
pub use status::{ClusterStatus, InstanceStatus, PoolerStatus};
pub use upgrade::{UpgradeOptions, UpgradeReport};

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

    #[error(transparent)]
    Backup(#[from] pgpod_backup::Error),

    #[error("pgbackrest {command} failed\n\n{detail}")]
    PgBackRest { command: String, detail: String },

    #[error("the {step} step of the upgrade failed\n\n{detail}")]
    Upgrade { step: String, detail: String },

    #[error(
        "the pgBackRest bundle is not installed at {0} — build it with \
         ops/build-pgbackrest.sh"
    )]
    PgBackRestMissing(String),

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

    #[error(
        "{instance} is running a different spec than the manifest describes, and \
         the spec is fixed when the container is created — a restart would \
         re-run the agent with the old one.\n\n{detail}\n\n\
         Applying this means replacing the container:\n\n  \
         pgpod apply -f <manifest> --recreate\n\n\
         With a pooler in front, clients are held across that window rather \
         than dropped (ADR 05). Without one, every open connection goes. \
         `pgpod delete <cluster>` followed by `pgpod apply` does the same \
         thing the long way and keeps the volume either way."
    )]
    Diverged { instance: String, detail: String },

    /// A pooler running an older spec.
    ///
    /// Its own variant rather than reusing `Diverged`: that message ends
    /// with "`pgpod delete <cluster>` keeps the volume and the data",
    /// which is advice about an instance. A pooler has no volume and no
    /// data, and pointing an operator at `pgpod delete <cluster>` to fix a
    /// pooler is pointing them at the wrong object entirely.
    #[error(
        "pooler {pooler} is running a different spec than the manifest \
         describes, and the spec is fixed when the container is created.\n\n\
         {detail}\n\n\
         Replacing it drops every connection through it — there is nothing in \
         front of a pooler to hold them — so pgpod will not do it \
         implicitly:\n  pgpod pooler delete {pooler}\n  pgpod apply -f \
         <manifest>\n\n\
         Nothing durable is destroyed: a pooler has no volume and re-renders \
         its config on every start."
    )]
    PoolerDiverged { pooler: String, detail: String },

    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The bootstrap superuser. `initdb` creates it and pgpod never renames it.
const SUPERUSER_ROLE: &str = "postgres";

/// What `ensure_secrets` settled, and what is left to do about it.
struct EnsuredSecrets {
    names: SecretNames,
    /// `(sql role name, which secret holds its password)` for roles whose
    /// password pgpod had to invent because the source's secret was not on
    /// this host. Empty in every case but that one.
    rotate: Vec<(String, String)>,
}

/// Which role a secret name belongs to, for a message an operator reads.
///
/// The podman secret name is `pgpod-<cluster>-<role>`, and quoting the
/// whole thing back at someone tells them about pgpod's naming rather
/// than about which password they need to reset.
fn role_of<'a>(secret: &'a str, cluster: &ClusterId) -> &'a str {
    // Strip the whole `pgpod-<cluster>-` prefix rather than splitting on
    // the last '-': the app owner's secret ends `-app-owner`, and taking
    // the final segment would report it as "owner".
    secret
        .strip_prefix(&format!("pgpod-{cluster}-"))
        .unwrap_or(secret)
}

/// The pgpod control plane.
pub struct Pgpod {
    podman: PodmanClient,
    registry: Registry,
    layout: PathLayout,
}

/// The things every instance in one `apply` shares.
///
/// Threaded through as a struct rather than as five more parameters: they
/// are computed once per apply and are the same for every instance, so
/// passing them individually invites a caller passing one instance's value
/// while creating another.
pub(crate) struct ApplyContext<'a> {
    bootstrap: Bootstrap,
    /// The cluster's podman subnet, for `pg_hba.conf`.
    network_cidr: Option<String>,
    secrets: SecretNames,
    agent: &'a std::path::Path,
    /// Whether a diverged instance may be recreated in place.
    recreate: bool,
    /// The ID `spec.imageName` resolves to on this host.
    ///
    /// Carried because the image is **not** part of [`InstanceSpec`] — it
    /// belongs to the container, not to the agent's instructions — so the
    /// spec comparison that catches every other manifest change cannot
    /// see it. Without this, editing `imageName` and applying reports
    /// "converged" and changes nothing.
    ///
    /// `None` only if the image vanished between the pull and here, in
    /// which case the comparison is skipped rather than guessed at.
    desired_image_id: Option<String>,
}

/// How one `apply` should behave.
///
/// A struct rather than more boolean parameters: `apply(m, wait, false)`
/// at a call site says nothing about what the `false` refuses to do, and
/// the thing it refuses to do here is an outage.
#[derive(Debug, Clone, Copy)]
pub struct ApplyOptions {
    /// How long to wait for instances to accept connections.
    pub wait: Duration,
    /// Recreate an instance whose running spec differs from the manifest.
    ///
    /// Off by default, because the spec travels in an environment variable
    /// fixed at container-create time, so applying a changed manifest
    /// means *replacing* the container. With a pooler in front that is a
    /// held pause rather than an outage; without one it drops every
    /// connection, which is why it is never implicit.
    pub recreate: bool,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            wait: Duration::from_secs(180),
            recreate: false,
        }
    }
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
    /// Poolers still fronting this cluster, which are now pointed at an
    /// instance that is not running.
    pub poolers: Vec<String>,
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
        self.apply_with(
            manifest,
            ApplyOptions {
                wait,
                ..Default::default()
            },
        )
        .await
    }

    /// Create or converge a cluster, choosing what divergence may do.
    pub async fn apply_with(
        &self,
        manifest: &ClusterManifest,
        options: ApplyOptions,
    ) -> Result<ApplyReport> {
        self.apply_with_bootstrap(
            manifest,
            Bootstrap::Initdb(manifest.spec.bootstrap.initdb.clone()),
            options,
        )
        .await
    }

    /// `apply`, with the bootstrap mode chosen by the caller.
    ///
    /// `restore` needs the whole of `apply` — network, secrets, volume,
    /// container, readiness — differing only in how PGDATA gets populated.
    /// Duplicating that would mean two paths to a running instance, and
    /// the restored one would be the less-exercised of the two.
    pub(crate) async fn apply_with_bootstrap(
        &self,
        manifest: &ClusterManifest,
        bootstrap: Bootstrap,
        options: ApplyOptions,
    ) -> Result<ApplyReport> {
        let wait = options.wait;
        let cluster = manifest.cluster_id()?;
        self.validate_parameters(manifest)?;

        let agent = self.agent_binary()?;

        let existed = self.registry.cluster(&cluster)?.is_some();

        // The image is the one part of a manifest that does **not** travel
        // in the instance spec, so the comparison that catches every other
        // change cannot see it. It is resolved to an ID here, once, and
        // compared against what each container was actually created from
        // — not against what pgpod recorded. The registry stores what the
        // last apply *asked for*, which is not the same thing: an apply
        // that failed on its image recorded that image anyway, and
        // trusting it made the retry see no change and repeat the failure
        // (found by doing exactly that).
        self.podman
            .pull_image_if_absent(&manifest.spec.image_name)
            .await?;
        let desired_image_id = self.podman.image_id(&manifest.spec.image_name).await?;

        // A changed image is the one manifest edit that can be a
        // major-version change, and `apply` cannot make one: starting a
        // newer PostgreSQL on an older data directory fails with
        // `database files are incompatible with server`, leaving an
        // instance that never becomes ready. Checked before anything is
        // created — and before the manifest is recorded, so a refusal
        // leaves nothing behind to confuse the next attempt.
        //
        // Not conditioned on the registry knowing this cluster: what
        // constrains the image is the *volume*, and a volume can outlive
        // the registry — a host that lost its SQLite still has its data
        // (ADR 04 §5). The check reads that volume and returns
        // immediately when there is nothing in it.
        self.refuse_incompatible_image(&cluster, manifest, desired_image_id.as_deref())
            .await?;

        let generation = self.registry.put_cluster(manifest, "applying")?;
        self.registry
            .record_event(cluster.as_str(), None, "info", "apply requested")?;

        // One network per cluster even at a single instance, so adding a
        // standby later does not have to move a running primary onto a
        // different network.
        let network = self
            .podman
            .ensure_network(&cluster.network_name(), &names::cluster_labels(&cluster))
            .await?;

        // Created before any instance starts, because archive_mode is
        // already `on` by then and the first segment can be archived
        // before `apply` returns. Never deleted implicitly: it holds the
        // backups (AGENTS.md principle 4).
        if let Some(volume) = &manifest.spec.backup.volume {
            self.podman
                .create_volume(volume, &names::cluster_labels(&cluster))
                .await?;
        }

        let EnsuredSecrets {
            names: secrets,
            rotate,
        } = self.ensure_secrets(&cluster, manifest, &bootstrap).await?;
        let ctx = ApplyContext {
            bootstrap,
            network_cidr: network.subnet.clone(),
            secrets,
            agent: &agent,
            recreate: options.recreate,
            desired_image_id: desired_image_id.clone(),
        };

        let mut summaries = Vec::new();
        for ordinal in 1..=manifest.spec.instances {
            let instance = cluster.instance(ordinal);
            let summary = self.apply_instance(manifest, &instance, &ctx).await?;
            summaries.push(summary);
        }

        if !wait.is_zero() {
            for ordinal in 1..=manifest.spec.instances {
                self.wait_ready(&cluster.instance(ordinal), wait).await?;
            }
            // After readiness, because ALTER ROLE needs a writable
            // cluster: a restore is still replaying, and then promoting,
            // right up until it is ready.
            self.rotate_restored_roles(&cluster, &rotate).await?;
        } else if !rotate.is_empty() {
            // Nothing has waited for the cluster to become writable, so
            // there is no moment at which the rotation could have run.
            // Said out loud rather than skipped quietly.
            self.registry.record_event(
                cluster.as_str(),
                None,
                "warn",
                "credentials were not rotated because apply did not wait for \
                 readiness — re-apply with a non-zero wait to make this \
                 cluster reachable",
            )?;
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
        ctx: &ApplyContext<'_>,
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
            Some(p) if p.running => match Self::refuse_if_diverged(manifest, instance, ctx, &p) {
                // Nothing changed. Recreating a healthy instance that
                // already matches the manifest would be a pointless
                // outage, so `--recreate` is permission, not instruction.
                Ok(()) => p.id,
                Err(e) if ctx.recreate => {
                    // Journalled before the container is touched: what
                    // changed is the operator's evidence that the outage
                    // was the one they asked for.
                    self.registry.record_event(
                        cluster.as_str(),
                        Some(instance.ordinal()),
                        "info",
                        &format!("spec diverged, recreating: {e}"),
                    )?;
                    self.recreate_instance(manifest, instance, ctx, host_port)
                        .await?
                }
                Err(e) => return Err(e),
            },
            Some(p) => match Self::refuse_if_diverged(manifest, instance, ctx, &p) {
                Ok(()) => {
                    self.podman.container(&p.id).start().await?;
                    p.id
                }
                Err(e) => {
                    // A **stopped** container that no longer matches the
                    // manifest is replaced without `--recreate` being
                    // asked for, because the reason that flag exists does
                    // not apply here: there is no outage to cause. This
                    // container is already down, and starting it would
                    // re-run the agent with the stale spec — the exact
                    // thing `refuse_if_diverged` exists to prevent.
                    //
                    // It is also the way back from a mistake. An `apply`
                    // that named an incompatible image leaves an exited
                    // container behind; putting the old image back in the
                    // manifest and applying it has to be enough, or the
                    // operator is told to `delete` a cluster in order to
                    // fix it.
                    self.registry.record_event(
                        cluster.as_str(),
                        Some(instance.ordinal()),
                        "info",
                        &format!("stopped instance diverged, recreating: {e}"),
                    )?;
                    let spec = self.container_spec(manifest, instance, ctx, host_port)?;
                    self.podman
                        .pull_image_if_absent(&manifest.spec.image_name)
                        .await?;
                    let handle = self.podman.container(&p.id);
                    handle.remove(true).await?;
                    let c = self.podman.create_container(&spec).await?;
                    c.start().await?;
                    c.id().to_string()
                }
            },
            None => {
                let spec = self.container_spec(manifest, instance, ctx, host_port)?;
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

    /// The spec the agent will act on.
    ///
    /// Separate from [`Self::container_spec`] because it is also what
    /// [`Self::instance_diverged`] compares against a running container:
    /// the two must be built by the same code, or the check would report
    /// drift that is only a difference in how the comparison was written.
    fn instance_spec(
        manifest: &ClusterManifest,
        instance: &InstanceId,
        ctx: &ApplyContext<'_>,
    ) -> Result<InstanceSpec> {
        // Presence of a destination is what turns archiving on. The switch
        // already exists in the renderer, so a cluster created *with* a
        // destination has `archive_mode = on` before postgres ever starts
        // — no restart, no special case (ADR 01 §1).
        let backup = pgpod_core::BackupSpec {
            destinations: manifest
                .spec
                .backup
                .destinations
                .iter()
                // Podman secret names mean nothing inside the container;
                // the agent reads credentials from an indexed mount.
                .map(|d| d.sanitized())
                .collect(),
            ..manifest.spec.backup.clone()
        };
        let archive_command = backup
            .is_enabled()
            .then(|| pgpod_pg::archive_command(instance.cluster().as_str()));

        let spec = InstanceSpec {
            instance: instance.clone(),
            port: 5432,
            bootstrap: ctx.bootstrap.clone(),
            parameters: manifest.parameters(),
            shared_preload_libraries: manifest.spec.postgresql.shared_preload_libraries.clone(),
            // Scoped to the cluster's own podman subnet, never guessed
            // and never wide open.
            //
            // Phase 1 left this `None` at one instance, on the reasoning
            // that a single-instance cluster has nobody to talk to. Base
            // backups made that false: the job container from ADR 01 §4
            // is a *separate* container on this network, connecting as
            // `streaming_replica`, and with no host rule PostgreSQL
            // rejects it — which is `pg_hba.conf` behaving correctly and
            // the backup failing for a reason nothing in the manifest
            // explains.
            network_cidr: ctx.network_cidr.clone(),
            archive_command,
            backup,
        };
        spec.validate()?;
        Ok(spec)
    }

    fn container_spec(
        &self,
        manifest: &ClusterManifest,
        instance: &InstanceId,
        ctx: &ApplyContext<'_>,
        host_port: u16,
    ) -> Result<ContainerSpec> {
        let spec = Self::instance_spec(manifest, instance, ctx)?;
        let (secrets, agent) = (&ctx.secrets, ctx.agent);

        let uid = manifest.spec.postgres_uid;
        let gid = manifest.spec.postgres_gid;

        let mut c: ContainerSpec = ContainerSpec::hardened(&manifest.spec.image_name)
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

        // pgBackRest, when this cluster archives. A directory rather than
        // the agent's single file, because it travels with its own
        // libraries and loader — which is what lets it run in an image it
        // was not built for (ADR 04 §2).
        if spec.backup.is_enabled() || spec.recovery().is_some() {
            c = c.mount(Mount::BindReadOnly {
                source: self.pgbackrest_bundle()?.display().to_string(),
                target: container::PGBACKREST_BUNDLE.to_string(),
            });
        }

        // The archive volume, when the cluster ships to a file://
        // destination. Mounted read-write and *outside* the instance's own
        // volume, so `pgpod delete --purge` cannot destroy the backups
        // along with the database they protect.
        //
        // A restored instance reads its source's archive, which is why the
        // recovery bootstrap's destinations are consulted too — the two
        // clusters may name different volumes.
        if let Some(volume) = archive_volume(manifest, &spec) {
            c = c.mount(Mount::Volume {
                name: volume,
                target: container::ARCHIVE_MOUNT.to_string(),
                chown: false,
            });
        }

        // `ensure_secrets` creates the app-owner secret only for a cluster
        // that asks for an application database, so mounting it
        // unconditionally makes podman refuse to create the container with
        // "no such secret" — a manifest without `bootstrap.initdb.database`
        // could never have started. The two conditions have to be the same
        // one, so they read from the same place.
        let mut mounts = vec![
            (&secrets.superuser, container::SECRET_SUPERUSER),
            (&secrets.replication, container::SECRET_REPLICATION),
            (&secrets.monitor, container::SECRET_MONITOR),
        ];
        if manifest.spec.bootstrap.initdb.database.is_some() {
            mounts.push((&secrets.app_owner, container::SECRET_APP_OWNER));
        }

        for (name, target) in mounts {
            c = c.secret(SecretMount {
                name: name.clone(),
                target: target.to_string(),
                mode: 0o400,
                uid,
                gid,
            });
        }

        // Object-store credentials, one mount per destination, addressed
        // by index. Absent is the normal case on GCE, where object_store
        // authenticates through the instance metadata server and there is
        // nothing to mount.
        for (index, dest) in manifest.spec.backup.destinations.iter().enumerate() {
            if let Some(secret) = &dest.credentials {
                c = c.secret(SecretMount {
                    name: secret.clone(),
                    target: pgpod_core::Destination::credentials_path(index),
                    mode: 0o400,
                    uid,
                    gid,
                });
            }
        }

        for (k, v) in names::instance_labels(instance) {
            c = c.label(k, v);
        }
        Ok(c)
    }

    /// Refuse to adopt a container that is running an older manifest.
    ///
    /// `apply` adopts a container that already exists rather than
    /// recreating it, which is right: recreating a healthy instance is a
    /// pointless outage. But the instance spec travels in an environment
    /// variable fixed at **container-create** time (`pgpod_core::spec`),
    /// so changing anything in it — parameters, preload libraries, a
    /// backup destination — needs the container *recreated*, not merely
    /// restarted. A restart re-runs the agent with the stale value.
    ///
    /// Nothing detected that until Phase 2, so `apply` would report
    /// success and change nothing. Adding a backup destination to a live
    /// cluster is the case that makes it dangerous rather than merely
    /// confusing: the operator sees "apply complete", believes their WAL
    /// is being archived, and finds out otherwise at the restore.
    ///
    /// **The image is not in that environment variable** — it belongs to
    /// the container, not to the agent's instructions — so it is compared
    /// separately, against the image ID this container was created from.
    /// It was missed entirely for the same reason it is easy to miss
    /// here: a spec comparison cannot see a field the spec does not have,
    /// and editing `imageName` therefore reported "converged" and changed
    /// nothing (ADR 06 §9).
    ///
    /// The remedy is `--recreate`, which holds clients at the pooler
    /// across the replacement (ADR 05). The version that knows
    /// `archive_command` is reloadable while `archive_mode` and
    /// `shared_preload_libraries` are restart-only belongs with the
    /// reconciler in Phase 4.
    fn refuse_if_diverged(
        manifest: &ClusterManifest,
        instance: &InstanceId,
        ctx: &ApplyContext<'_>,
        probe: &pgpod_runtime::ContainerProbe,
    ) -> Result<()> {
        let Some(running) = probe.env_var(SPEC_ENV) else {
            // No spec in the environment at all: not a container pgpod
            // created, or one from before the spec was carried this way.
            // Adopting it blind would be worse than saying so.
            return Err(Error::Diverged {
                instance: instance.to_string(),
                detail: format!("the running container has no {SPEC_ENV}"),
            });
        };

        let desired = Self::instance_spec(manifest, instance, ctx)?;
        // Compared as parsed values, not as strings: JSON key order and
        // whitespace are not manifest changes, and reporting them as such
        // would train operators to ignore this error.
        let running: InstanceSpec = serde_json::from_str(running)
            .map_err(|e| Error::Spec(pgpod_core::SpecError::Malformed(e.to_string())))?;

        // The image is not in the spec — it belongs to the container, not
        // to the agent's instructions — so it is compared separately,
        // against what this container was actually created from. Left
        // out, a changed `imageName` was adopted silently and `apply`
        // reported success for a manifest it had not applied.
        let image_change = match (&ctx.desired_image_id, &probe.image_id) {
            (Some(desired), Some(running)) if desired != running => Some((
                probe
                    .image_name
                    .clone()
                    .unwrap_or_else(|| short_id(running)),
                manifest.spec.image_name.clone(),
            )),
            _ => None,
        };

        if running == desired && image_change.is_none() {
            return Ok(());
        }

        Err(Error::Diverged {
            instance: instance.to_string(),
            detail: differences(&running, &desired, image_change.as_ref()).join("\n"),
        })
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
        bootstrap: &Bootstrap,
    ) -> Result<EnsuredSecrets> {
        let names = SecretNames::for_cluster(cluster);
        let wants_app = manifest.spec.bootstrap.initdb.database.is_some();

        // A restored cluster does not get fresh credentials, because it
        // does not get fresh roles. `pg_authid` comes out of the backup
        // carrying the source's, and the restore path deliberately does
        // not rewrite them — that is the whole point of restoring a
        // database "as it was". Generating passwords here produced secrets
        // no role has: `pgpod status` printed a `postgresql://app@…` URI
        // that could not be used, and nothing said why, because
        // `pgpod psql` goes over the unix socket with `peer` and never
        // touched them (ADR 04 §8).
        let source = match bootstrap {
            Bootstrap::Recovery(r) => Some(
                ClusterId::new(r.source_stanza.clone())
                    .map_err(|e| Error::Invalid(format!("recovery source: {e}")))?,
            ),
            Bootstrap::Initdb(_) => None,
        };
        let source_names = source.as_ref().map(SecretNames::for_cluster);

        let mut unavailable = Vec::new();
        for (name, from) in match &source_names {
            Some(s) => names.paired(s),
            // Paired with itself when there is no source: the loop below
            // never consults `from` unless `source_names` is `Some`.
            None => names.paired(&names),
        } {
            if name == names.app_owner && !wants_app {
                continue;
            }
            if self.podman.secret_exists(name).await? {
                continue;
            }

            let adopted = match &source_names {
                Some(_) => self.podman.secret_value(from).await?,
                None => None,
            };
            match adopted {
                Some(value) => {
                    self.podman.put_secret(name, &value).await?;
                }
                None => {
                    if source_names.is_some() {
                        // The source's secret is not on this host — a
                        // restore onto a fresh machine, which is a
                        // supported and deliberate path (ADR 04 §5). The
                        // password in the restored database came from the
                        // backup and nobody here knows it, so the role is
                        // rotated to this generated one once the cluster
                        // is writable. Recorded by role name, not by
                        // secret name: it is the thing that gets altered.
                        unavailable.push(role_of(name, cluster).to_string());
                    }
                    let generated: Secret = password::generate();
                    self.podman.put_secret(name, generated.expose()).await?;
                }
            }
        }

        let rotate = if unavailable.is_empty() {
            Vec::new()
        } else {
            let from = source
                .as_ref()
                .map(ClusterId::to_string)
                .unwrap_or_default();
            self.registry.record_event(
                cluster.as_str(),
                None,
                "warn",
                &format!(
                    "restored from {from}, whose secrets are not on this host — \
                     rotating {} to freshly generated passwords so the cluster \
                     is reachable",
                    unavailable.join(", ")
                ),
            )?;
            // Translate each secret's role into the SQL role to alter.
            // The app owner's name comes from the manifest; the rest are
            // fixed by pgpod.
            unavailable
                .iter()
                .filter_map(|which| {
                    let role = match which.as_str() {
                        "superuser" => Some(SUPERUSER_ROLE.to_string()),
                        "replication" => Some(pgpod_pg::REPLICATION_ROLE.to_string()),
                        "monitor" => Some(pgpod_pg::MONITOR_ROLE.to_string()),
                        "app-owner" => manifest.spec.bootstrap.initdb.owner.clone(),
                        _ => None,
                    }?;
                    Some((role, which.clone()))
                })
                .collect()
        };

        Ok(EnsuredSecrets { names, rotate })
    }

    /// Rotate roles whose password came out of a backup nobody here holds.
    ///
    /// Runs once the instance is accepting connections, over the unix
    /// socket as the container's own user — `peer`, so it needs no
    /// password, which is the only reason this is possible at all.
    ///
    /// A failure fails the restore. A cluster whose data is correct and
    /// whose credentials are unknown is not a successful restore, and
    /// reporting it as one is how an operator finds out during the next
    /// incident instead of this one.
    async fn rotate_restored_roles(
        &self,
        cluster: &ClusterId,
        rotate: &[(String, String)],
    ) -> Result<()> {
        if rotate.is_empty() {
            return Ok(());
        }
        let names = SecretNames::for_cluster(cluster);
        let instance = cluster.instance(1);
        let container = self.running_container(&instance).await?;

        // **Readiness is not writability.** `wait_ready` polls
        // `pg_isready`, which succeeds as soon as the postmaster accepts
        // connections — and a restoring cluster accepts them while it is
        // still replaying, promoting only once it reaches its target. Left
        // out, the rotation raced the promotion and lost:
        // `ERROR: cannot execute ALTER ROLE in a read-only transaction`.
        // It passed locally against a small archive and failed on the
        // deployment target against a real one, which is the way this
        // class of bug usually arrives.
        self.wait_writable(&instance, Duration::from_secs(300))
            .await?;

        for (role, which) in rotate {
            let secret = match which.as_str() {
                "superuser" => &names.superuser,
                "replication" => &names.replication,
                "monitor" => &names.monitor,
                _ => &names.app_owner,
            };
            let password = self
                .podman
                .secret_value(secret)
                .await?
                .map(Secret::new)
                .ok_or_else(|| {
                    Error::Invalid(format!("secret {secret} vanished between create and use"))
                })?;

            let sql = pgpod_pg::alter_role_password_sql(role, &password)?;
            let out = container
                .exec(&ExecSpec::new([
                    "psql",
                    "-X",
                    "-q",
                    "-v",
                    "ON_ERROR_STOP=1",
                    "-h",
                    container::SOCKET_DIR,
                    "-U",
                    "postgres",
                    "-d",
                    "postgres",
                    "-c",
                    &sql,
                ]))
                .await?;
            if !out.success() {
                // The statement carries the new password; report only what
                // postgres said.
                return Err(Error::Invalid(format!(
                    "could not rotate the {role:?} password on restored cluster \
                     {cluster}, so it would not be reachable with the \
                     credentials pgpod stores: {}",
                    out.stderr.trim()
                )));
            }
            self.registry.record_event(
                cluster.as_str(),
                Some(1),
                "info",
                &format!("rotated the {role} password after restore"),
            )?;
        }
        Ok(())
    }

    /// Wait until the instance is out of recovery and accepting writes.
    ///
    /// Distinct from [`Self::wait_ready`], and deliberately not folded
    /// into it: a standby is read-only for its whole life, so "accepting
    /// connections" is the right readiness test in general. This is for
    /// the one path that must then write — a restore, which promotes at
    /// its recovery target.
    async fn wait_writable(&self, instance: &InstanceId, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let container = self.podman.container(instance.container_name());
        let mut last = String::new();

        while std::time::Instant::now() < deadline {
            let probe = ExecSpec::new([
                "psql",
                "-X",
                "-tA",
                "-h",
                container::SOCKET_DIR,
                "-U",
                "postgres",
                "-c",
                "SELECT pg_is_in_recovery()",
            ]);
            match container.exec(&probe).await {
                Ok(out) if out.success() && out.stdout.trim() == "f" => return Ok(()),
                Ok(out) if out.success() => last = "still in recovery".to_string(),
                Ok(out) => last = format!("{}{}", out.stdout.trim(), out.stderr.trim()),
                Err(e) => last = e.to_string(),
            }
            if let Some(p) = container.probe().await?
                && !p.running
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        Err(Error::NotReady {
            instance: instance.to_string(),
            seconds: timeout.as_secs(),
            detail: format!("never left recovery: {last}"),
        })
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

            // A container that has exited is never going to become ready,
            // and polling it until the deadline turns a clear failure into
            // a long hang. It matters most on the path with the longest
            // timeout: a restore whose recovery target is unreachable
            // FATALs in seconds, and waiting half an hour to say so would
            // leave an operator watching a blank terminal during exactly
            // the incident they are trying to recover from.
            if let Some(probe) = container.probe().await?
                && !probe.running
            {
                return Err(self.not_ready(instance, &container, timeout, &last).await);
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        Err(self.not_ready(instance, &container, timeout, &last).await)
    }

    /// Build the "did not come up" error, with the reason attached.
    ///
    /// Container logs are where the real cause lives — a failed initdb, a
    /// bad parameter, a missing secret, or a recovery target the archive
    /// cannot reach.
    async fn not_ready(
        &self,
        instance: &InstanceId,
        container: &pgpod_runtime::Container,
        timeout: Duration,
        last_probe: &str,
    ) -> Error {
        let logs = container
            .logs_string()
            .await
            .unwrap_or_else(|e| format!("<logs unavailable: {e}>"));
        Error::NotReady {
            instance: instance.to_string(),
            seconds: timeout.as_secs(),
            detail: format!("last probe: {last_probe}\n\ncontainer logs:\n{logs}"),
        }
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

        // **Checked before anything is removed.** The registry refuses to
        // drop a cluster row a pooler still references, but that check
        // sits at the *end* of this function — so a `--purge` would remove
        // the containers, destroy the volumes, and only then report that
        // it had refused. Found by a test that deleted a fronted cluster
        // and then could not delete it again, because there was nothing
        // left to delete.
        //
        // Only `--purge` is refused. A plain delete keeps the volumes and
        // a later apply brings the instance back under the same name,
        // which the pooler picks up on its own — `server_host` is
        // re-resolved per backend connect. Destroying the data underneath
        // a live pooler is the one that cannot be undone.
        let poolers: Vec<String> = self
            .registry
            .poolers_for_cluster(cluster)?
            .into_iter()
            .map(|p| p.name)
            .collect();
        if purge && !poolers.is_empty() {
            return Err(Error::Invalid(format!(
                "cluster {cluster} is still fronted by pooler {}, and --purge \
                 destroys its data. Delete the pooler first \
                 (`pgpod pooler delete {}`), or drop --purge to keep the \
                 volumes.",
                poolers.join(", "),
                poolers[0],
            )));
        }

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

        // Instance volumes only. The archive volume is deliberately not
        // touched even by `--purge`: it holds the backups, and destroying
        // those together with the database they protect is the one thing
        // an operator would never mean by "delete this cluster".
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

        // The network holds no data, and `remove_network` is the
        // non-forcing call, so podman refuses while anything is still
        // attached — a pooler fronting this cluster, most likely. Ignoring
        // the error is the whole intent: the network outliving a delete is
        // correct when something is still using it.
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
            poolers,
        })
    }
}

/// The podman volume an instance needs mounted at
/// `container::ARCHIVE_MOUNT`, if any.
///
/// One field covers both the ordinary and the restored case: `restore`
/// builds the target cluster's manifest from the source's, so a restored
/// instance names the same volume it will read the base backup out of. The
/// spec is passed in so this cannot silently succeed for an instance whose
/// recovery bootstrap points somewhere the manifest does not.
fn archive_volume(manifest: &ClusterManifest, spec: &InstanceSpec) -> Option<String> {
    let volume = manifest.spec.backup.volume.clone();
    if volume.is_none()
        && let Some(recovery) = spec.recovery()
        && recovery
            .destinations
            .iter()
            .any(|d| d.url.trim().starts_with("file://"))
    {
        // Unreachable through `restore`, which copies the manifest. It
        // would mean a hand-built spec, and the instance would fail on its
        // first restore_command with a confusing "not found".
        tracing::warn!(
            "instance restores from a file:// archive but its manifest names no \
             spec.backup.volume — the archive will not be mounted"
        );
    }
    volume
}

/// An image ID, shortened the way podman prints one.
///
/// Only ever for a message: an image with no name is still an image, and
/// 64 hex characters in an error tells an operator less than 12 do.
fn short_id(id: &str) -> String {
    id.trim_start_matches("sha256:").chars().take(12).collect()
}

/// Name what changed between two specs.
///
/// A diff rather than "they differ": the operator has to decide whether a
/// recreate is worth an outage, and cannot without knowing what moved.
fn differences(
    running: &InstanceSpec,
    desired: &InstanceSpec,
    image_change: Option<&(String, String)>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut note = |field: &str, from: String, to: String| {
        if from != to {
            out.push(format!("  {field}: {from} -> {to}"));
        }
    };

    if let Some((from, to)) = image_change {
        note("image", from.clone(), to.clone());
    }

    note(
        "bootstrap",
        format!("{:?}", running.bootstrap),
        format!("{:?}", desired.bootstrap),
    );
    note(
        "parameters",
        format!("{:?}", running.parameters),
        format!("{:?}", desired.parameters),
    );
    note(
        "sharedPreloadLibraries",
        format!("{:?}", running.shared_preload_libraries),
        format!("{:?}", desired.shared_preload_libraries),
    );
    note(
        "archive_mode",
        archive_mode_of(running).to_string(),
        archive_mode_of(desired).to_string(),
    );
    note(
        "backup.destinations",
        destination_urls(running),
        destination_urls(desired),
    );
    note(
        "backup.retention",
        format!(
            "{:?} ({:?})",
            running.backup.retention, running.backup.retention_mode
        ),
        format!(
            "{:?} ({:?})",
            desired.backup.retention, desired.backup.retention_mode
        ),
    );

    if out.is_empty() {
        // Equality already failed, so something changed that this diff
        // does not name. Say that, rather than printing an empty list and
        // looking like a bug.
        out.push("  (a field this diff does not yet name)".to_string());
    }
    out
}

fn archive_mode_of(spec: &InstanceSpec) -> &'static str {
    if spec.archive_command.is_some() {
        "on"
    } else {
        "off"
    }
}

fn destination_urls(spec: &InstanceSpec) -> String {
    if spec.backup.destinations.is_empty() {
        return "none".to_string();
    }
    spec.backup
        .destinations
        .iter()
        .map(|d| d.url.as_str())
        .collect::<Vec<_>>()
        .join(", ")
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
    #[test]
    fn a_secret_name_reduces_to_the_role_an_operator_must_reset() {
        // The message names the password to fix, not pgpod's naming
        // scheme. "pgpod-mydb-restored-app-owner" is not an answer to
        // "which password is wrong".
        let mydb = ClusterId::new("mydb").unwrap();
        assert_eq!(role_of("pgpod-mydb-superuser", &mydb), "superuser");
        assert_eq!(
            role_of("pgpod-mydb-app-owner", &mydb),
            "app-owner",
            "splitting on the last '-' would report this as \"owner\""
        );
        // A hyphenated cluster name must not confuse the prefix either.
        let hyphen = ClusterId::new("my-db").unwrap();
        assert_eq!(role_of("pgpod-my-db-monitor", &hyphen), "monitor");
        assert_eq!(role_of("unexpected", &mydb), "unexpected");
    }

    use super::*;

    fn manifest(yaml: &str) -> ClusterManifest {
        ClusterManifest::from_yaml(yaml).unwrap()
    }

    #[test]
    fn a_changed_image_is_named_in_the_diff() {
        // The image is not in the instance spec, so it is the one
        // manifest change the spec comparison cannot see. An operator who
        // edits `imageName` has to read what pgpod thinks changed and
        // recognise their own edit in it.
        let spec = |port: u16| InstanceSpec {
            instance: ClusterId::new("mydb").unwrap().instance(1),
            port,
            bootstrap: Bootstrap::Initdb(Default::default()),
            parameters: Vec::new(),
            shared_preload_libraries: Vec::new(),
            network_cidr: None,
            archive_command: None,
            backup: Default::default(),
        };
        let change = (
            "docker.io/library/postgres:17".to_string(),
            "docker.io/library/postgres:18".to_string(),
        );
        let out = differences(&spec(5432), &spec(5432), Some(&change)).join("\n");
        assert!(
            out.contains("image: docker.io/library/postgres:17 -> docker.io/library/postgres:18"),
            "{out}"
        );

        // And it is the *only* thing reported when it is the only change:
        // a diff that also listed identical fields would train operators
        // to skim past it.
        assert_eq!(out.lines().count(), 1, "{out}");
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
