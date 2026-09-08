//! Assembling what `pgpod status` reports.
//!
//! Every field that can be read live *is* read live. The registry supplies
//! names and ports; podman supplies whether a container is running; the
//! database supplies its own role. A status built only from stored rows
//! would confidently report a cluster that had been down for a week.

use pgpod_core::{ClusterId, InstancePhase, InstanceRole};
use pgpod_runtime::ExecSpec;
use serde::Serialize;

use crate::{Error, Pgpod, Result, connection_uri};

#[derive(Debug, Clone, Serialize)]
pub struct ClusterStatus {
    pub cluster: String,
    pub generation: i64,
    pub phase: String,
    pub image: String,
    pub instances: Vec<InstanceStatus>,
    /// Advisory only — see `StorageSpec::size`.
    pub storage_size: Option<String>,
    pub recent_events: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstanceStatus {
    pub instance: String,
    pub container: String,
    pub volume: String,
    pub host_port: u16,
    /// What pgpod intends.
    pub phase: InstancePhase,
    /// What podman reports right now. `None` means podman has never heard
    /// of the container — the row is stale.
    pub container_state: Option<String>,
    pub running: bool,
    /// What the database says about itself, never what the registry
    /// remembered (ADR 02 §7). `Unknown` when it could not be asked.
    pub role: InstanceRole,
    pub accepting_connections: bool,
    pub connection_uri: String,
}

pub(crate) async fn collect(pgpod: &Pgpod, cluster: &ClusterId) -> Result<ClusterStatus> {
    let record = pgpod.registry().require_cluster(cluster)?;
    let rows = pgpod.registry().instances(cluster)?;

    let mut instances = Vec::new();
    for row in rows {
        let handle = pgpod.podman().container(&row.container_name);
        let probe = handle.probe().await?;
        let running = probe.as_ref().map(|p| p.running).unwrap_or(false);

        // Only ask the database if the container is actually up —
        // otherwise every stopped instance costs an exec timeout.
        let (role, accepting) = if running {
            match handle
                .exec(&ExecSpec::new([
                    "psql",
                    "-X",
                    "-tA",
                    "-h",
                    pgpod_core::container::SOCKET_DIR,
                    "-U",
                    "postgres",
                    "-c",
                    "SELECT pg_is_in_recovery()",
                ]))
                .await
            {
                Ok(out) if out.success() => (
                    InstanceRole::from_in_recovery(out.stdout.trim() == "t"),
                    true,
                ),
                // Running but not answering: starting up, recovering, or
                // broken. Not a role we may assume.
                _ => (InstanceRole::Unknown, false),
            }
        } else {
            (InstanceRole::Unknown, false)
        };

        instances.push(InstanceStatus {
            instance: format!("{}-{}", row.cluster, row.ordinal),
            container: row.container_name.clone(),
            volume: row.volume_name.clone(),
            host_port: row.host_port,
            phase: row.phase,
            container_state: probe.and_then(|p| p.status),
            running,
            role,
            accepting_connections: accepting,
            connection_uri: connection_uri(row.host_port, &record.manifest),
        });
    }

    let recent_events = pgpod
        .registry()
        .recent_events(cluster, 10)?
        .into_iter()
        .map(|e| format!("[{}] {}", e.level, e.message))
        .collect();

    Ok(ClusterStatus {
        cluster: record.name.clone(),
        generation: record.generation,
        phase: record.phase.clone(),
        image: record.manifest.spec.image_name.clone(),
        storage_size: record.manifest.spec.storage.size.clone(),
        instances,
        recent_events,
    })
}

impl ClusterStatus {
    /// The primary's URI, when there is a confirmed primary.
    ///
    /// `None` rather than a guess: an instance whose role could not be
    /// read must not be handed out as the write endpoint.
    pub fn primary_uri(&self) -> Option<&str> {
        self.instances
            .iter()
            .find(|i| i.role == InstanceRole::Primary)
            .map(|i| i.connection_uri.as_str())
    }
}

impl Pgpod {
    /// A container handle for an instance, running or not.
    ///
    /// Distinct from [`Pgpod::running_container`]: `pgpod logs` must work
    /// on a container that has exited, which is exactly when its logs
    /// matter most.
    /// The podman client, for tests and embedders that need to reach
    /// something pgpod does not model — an archive volume, for instance.
    pub fn podman_client(&self) -> &pgpod_runtime::PodmanClient {
        self.podman()
    }

    pub fn podman_container(&self, instance: &pgpod_core::InstanceId) -> pgpod_runtime::Container {
        self.podman().container(instance.container_name())
    }

    /// Look up one of pgpod's volumes.
    pub async fn podman_volume(&self, name: &str) -> Result<Option<pgpod_runtime::VolumeInfo>> {
        Ok(self.podman().volume(name).await?)
    }

    /// Resolve an instance name like `mydb-1` to a running container.
    pub async fn running_container(
        &self,
        instance: &pgpod_core::InstanceId,
    ) -> Result<pgpod_runtime::Container> {
        let handle = self.podman().container(instance.container_name());
        match handle.probe().await? {
            Some(p) if p.running => Ok(handle),
            Some(p) => Err(Error::Invalid(format!(
                "instance {instance} is not running (state: {})",
                p.status.unwrap_or_else(|| "unknown".into())
            ))),
            None => Err(Error::Invalid(format!(
                "instance {instance} has no container — has it been created?"
            ))),
        }
    }
}
