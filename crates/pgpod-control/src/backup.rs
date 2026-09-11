//! Taking a backup, listing them, and restoring one.
//!
//! All three are the same shape: run pgBackRest in a short-lived container
//! that mounts the instance's volume and the pgBackRest bundle. pgpod
//! decides *when* and *which*; pgBackRest does the work (ADR 04).
//!
//! The job container is strikingly small compared with what ADR 01 §4
//! needed. It takes no replication credential, joins no network, and needs
//! no `pg_basebackup`: pgBackRest reads PGDATA from the shared volume and
//! reaches PostgreSQL over the unix socket that lives in that same volume
//! (`container::SOCKET_DIR`). Two containers sharing one volume share the
//! socket in it, so there is nothing to configure.

use std::time::Duration;

use pgpod_backup::{Backup, StanzaInfo};
use pgpod_core::{ClusterId, ClusterManifest, Destination, RecoveryBootstrap, container};
use pgpod_runtime::{ContainerSpec, Mount, SecretMount};

use crate::{Error, Pgpod, Result};

/// What `backup` did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupReport {
    pub cluster: String,
    /// pgBackRest's label for the new backup, once it exists.
    pub label: Option<String>,
    pub destinations: Vec<String>,
    /// `full`, `diff` or `incr`, as pgBackRest chose.
    pub backup_type: Option<String>,
    pub size_bytes: Option<u64>,
}

/// What `restore` did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RestoreReport {
    pub source_cluster: String,
    pub target_cluster: String,
    pub backup_label: String,
    pub target_time: Option<String>,
    pub instances: Vec<crate::InstanceSummary>,
}

impl Pgpod {
    /// Take a backup of `cluster`.
    ///
    /// pgBackRest picks full/differential/incremental itself based on what
    /// the repository already holds, which is one of the things pgpod's
    /// own implementation never had.
    pub async fn backup(&self, cluster: &ClusterId, wait: Duration) -> Result<BackupReport> {
        let record = self.registry.require_cluster(cluster)?;
        let manifest = record.manifest;

        if !manifest.spec.backup.is_enabled() {
            return Err(Error::Invalid(format!(
                "{cluster} has no spec.backup.destinations, so there is nowhere \
                 to put a backup"
            )));
        }

        let before = self.stanza_info(cluster).await?;

        self.registry
            .record_event(cluster.as_str(), Some(1), "info", "backup started")?;

        let name = format!("pgpod-{cluster}-backup-{}", now_stamp());
        let out = self
            .run_pgbackrest(&manifest, cluster, &name, &["backup"], wait)
            .await?;

        if let Err(e) = out {
            self.registry
                .record_event(cluster.as_str(), Some(1), "error", "backup failed")?;
            return Err(e);
        }

        // The repository is the record of what happened, not the job's exit
        // code — which only says the process finished (ADR 04 §5).
        let after = self.stanza_info(cluster).await?;
        let new = after
            .as_ref()
            .map(|s| s.backups_oldest_first())
            .unwrap_or_default()
            .into_iter()
            .rfind(|b| {
                !before
                    .as_ref()
                    .map(|s| s.backup.iter().any(|old| old.label == b.label))
                    .unwrap_or(false)
            });

        self.registry
            .record_event(cluster.as_str(), Some(1), "info", "backup complete")?;

        Ok(BackupReport {
            cluster: cluster.to_string(),
            label: new.as_ref().map(|b| b.label.clone()),
            backup_type: new
                .as_ref()
                .map(|b| format!("{:?}", b.backup_type).to_lowercase()),
            size_bytes: new.as_ref().map(Backup::size_bytes),
            destinations: manifest
                .spec
                .backup
                .destinations
                .iter()
                .map(|d| d.url.clone())
                .collect(),
        })
    }

    /// The backups pgBackRest holds for this cluster, oldest first.
    pub async fn backups(&self, cluster: &ClusterId) -> Result<Vec<Backup>> {
        Ok(self
            .stanza_info(cluster)
            .await?
            .map(|s| s.backups_oldest_first())
            .unwrap_or_default())
    }

    /// Ask the repository what it holds.
    ///
    /// Read from pgBackRest rather than from pgpod's registry, which is
    /// the whole point of ADR 04 §5: the repository is self-describing, so
    /// a host that lost its SQLite can still see what is restorable.
    pub(crate) async fn stanza_info(&self, cluster: &ClusterId) -> Result<Option<StanzaInfo>> {
        let record = self.registry.require_cluster(cluster)?;
        if !record.manifest.spec.backup.is_enabled() {
            return Ok(None);
        }

        let name = format!("pgpod-{cluster}-info-{}", now_stamp());
        let out = self
            .run_pgbackrest(
                &record.manifest,
                cluster,
                &name,
                &["info", "--output=json"],
                Duration::from_secs(120),
            )
            .await?;

        let logs = out?;
        // pgBackRest writes its own log lines to stderr and the JSON
        // document to stdout, but podman's log stream carries both. The
        // document is one line and starts with `[`, so it is findable
        // without depending on stream separation.
        let json = logs
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with('['))
            .ok_or_else(|| Error::Invalid(format!("pgbackrest info produced no JSON:\n{logs}")))?;

        Ok(pgpod_backup::stanza(json, cluster.as_str())?)
    }

    /// Restore `source` into a new cluster, optionally at a point in time.
    pub async fn restore(
        &self,
        source: &ClusterId,
        target: &ClusterId,
        at: Option<chrono::DateTime<chrono::Utc>>,
        wait: Duration,
    ) -> Result<RestoreReport> {
        if source == target {
            // Restoring over the source would replay the repository onto
            // the running cluster's own volume.
            return Err(Error::Invalid(format!(
                "{source} cannot be restored over itself — give --as a new \
                 cluster name"
            )));
        }
        if self.registry.cluster(target)?.is_some() {
            return Err(Error::Invalid(format!(
                "{target} already exists — delete it first, or pick another \
                 --as name"
            )));
        }

        let record = self.registry.require_cluster(source)?;
        let source_manifest = record.manifest;
        if !source_manifest.spec.backup.is_enabled() {
            return Err(Error::Invalid(format!(
                "{source} has no spec.backup.destinations, so there is nothing \
                 to restore from"
            )));
        }

        let info = self
            .stanza_info(source)
            .await?
            .ok_or_else(|| Error::Invalid(format!("{source} has no pgbackrest repository yet")))?;

        let chosen = info.backup_for_target(at).ok_or_else(|| {
            let available = info
                .backups_oldest_first()
                .iter()
                .map(|b| format!("{} (ended {})", b.label, b.ended_at()))
                .collect::<Vec<_>>()
                .join(", ");
            Error::Invalid(format!(
                "no backup of {source} finished at or before this target.\n\
                 Backups: {}\n\
                 A target earlier than the oldest backup cannot be reached.",
                if available.is_empty() {
                    "none — take one with `pgpod backup`".to_string()
                } else {
                    available
                }
            ))
        })?;

        // The restored cluster is a real cluster with its own name, its own
        // stanza and its own secrets. `bootstrap.initdb` is inherited
        // rather than cleared: it is not used to create anything — the
        // bootstrap mode below is `Recovery` — but it names the application
        // database and owner, which the restored cluster has because they
        // came out of the backup.
        let mut target_manifest = source_manifest.clone();
        target_manifest.metadata.name = target.to_string();

        let recovery = RecoveryBootstrap {
            source_stanza: source.to_string(),
            backup_label: Some(chosen.label.clone()),
            target_time: at.map(|t| t.to_rfc3339()),
            destinations: source_manifest
                .spec
                .backup
                .destinations
                .iter()
                .map(Destination::sanitized)
                .collect(),
        };

        let report = self
            .apply_with_bootstrap(
                &target_manifest,
                pgpod_core::Bootstrap::Recovery(recovery),
                // A restore creates a cluster that does not exist yet, so
                // there is nothing to recreate and no pooler in front of
                // it to hold.
                crate::ApplyOptions {
                    wait,
                    recreate: false,
                },
            )
            .await?;

        Ok(RestoreReport {
            source_cluster: source.to_string(),
            target_cluster: target.to_string(),
            backup_label: chosen.label.clone(),
            target_time: at.map(|t| t.to_rfc3339()),
            instances: report.instances,
        })
    }

    /// Run one pgBackRest command in a throwaway container.
    ///
    /// Returns the container's output on success, so a caller can parse it
    /// — and the same output inside the error on failure, because
    /// pgBackRest's diagnosis is the useful part.
    ///
    /// The outer `Result` is pgpod failing to run the container at all; the
    /// inner one is pgBackRest failing. They need different fixes.
    async fn run_pgbackrest(
        &self,
        manifest: &ClusterManifest,
        cluster: &ClusterId,
        name: &str,
        args: &[&str],
        wait: Duration,
    ) -> Result<Result<String>> {
        let instance = cluster.instance(1);
        let record = self
            .registry
            .instance(cluster, 1)?
            .ok_or_else(|| Error::Invalid(format!("{cluster} has no instance")))?;

        let spec = self.pgbackrest_container_spec(manifest, cluster, name, args, &record)?;
        let container = self.podman.create_container(&spec).await?;
        container.start().await?;

        let code = tokio::time::timeout(wait, container.wait_for_exit())
            .await
            .map_err(|_| Error::NotReady {
                instance: instance.to_string(),
                seconds: wait.as_secs(),
                detail: format!("pgbackrest {} is still running", args.join(" ")),
            })??;

        // Read before removing, or the reason for a failure goes with it.
        let logs = container
            .logs_string()
            .await
            .unwrap_or_else(|e| format!("<logs unavailable: {e}>"));
        let _ = container.remove(true).await;

        if code != 0 {
            return Ok(Err(Error::PgBackRest {
                command: args.join(" "),
                detail: logs,
            }));
        }
        Ok(Ok(logs))
    }

    fn pgbackrest_container_spec(
        &self,
        manifest: &ClusterManifest,
        cluster: &ClusterId,
        name: &str,
        args: &[&str],
        instance: &pgpod_registry::InstanceRecord,
    ) -> Result<ContainerSpec> {
        let uid = manifest.spec.postgres_uid;
        let gid = manifest.spec.postgres_gid;
        let bundle = self.pgbackrest_bundle()?;

        let mut argv = vec![
            container::PGBACKREST_BIN.to_string(),
            format!("--config={}", container::PGBACKREST_CONF),
            format!("--stanza={cluster}"),
        ];
        argv.extend(args.iter().map(|a| a.to_string()));

        let mut c = ContainerSpec::hardened(&manifest.spec.image_name)
            .name(name)
            // pgBackRest refuses to run as root, and so does PostgreSQL —
            // the same uid for the same reason (ADR 04 §2).
            .user(format!("{uid}:{gid}"))
            .entrypoint(argv)
            // The cluster network. PostgreSQL is reached over the unix
            // socket in the shared volume, so this is not for that — it is
            // for the *repository*: an s3:// or gs:// endpoint has to be
            // resolvable, and a job container on no network cannot resolve
            // anything. A posix repository needs neither, which is exactly
            // why leaving this out passed every local test.
            .network(cluster.network_name())
            // This container's stdout is a *return value*, not a log:
            // `pgbackrest info --output=json` is parsed by the control
            // plane. Instance containers deliberately inherit the host's
            // driver — journald on the deployment target — but routing a
            // machine-readable document through the system journal would
            // put noise in the operator's journal and risk journald's
            // 48 KiB LineMax silently truncating it. The container lives
            // for seconds and is removed straight after.
            .log_driver("k8s-file")
            // The instance's volume: PGDATA to read, the rendered
            // pgbackrest.conf to obey, and the postgres socket to connect
            // through. One mount covers all three.
            .mount(Mount::Volume {
                name: instance.volume_name.clone(),
                target: container::VOLUME_MOUNT.to_string(),
                chown: false,
            })
            .mount(Mount::BindReadOnly {
                source: bundle.display().to_string(),
                target: container::PGBACKREST_BUNDLE.to_string(),
            });

        if let Some(volume) = &manifest.spec.backup.volume {
            c = c.mount(Mount::Volume {
                name: volume.clone(),
                target: container::ARCHIVE_MOUNT.to_string(),
                chown: false,
            });
        }

        for (index, dest) in manifest.spec.backup.destinations.iter().enumerate() {
            if let Some(secret) = &dest.credentials {
                c = c.secret(SecretMount {
                    name: secret.clone(),
                    target: Destination::credentials_path(index),
                    mode: 0o400,
                    uid,
                    gid,
                });
            }
        }

        c = c
            .label(crate::LABEL_CLUSTER, cluster.as_str())
            .label("pgpod.job", "pgbackrest");
        Ok(c)
    }

    pub(crate) fn agent_binary(&self) -> Result<std::path::PathBuf> {
        let agent = self.layout.agent_binary_for_host();
        if !agent.exists() {
            return Err(Error::AgentMissing(agent.display().to_string()));
        }
        Ok(agent)
    }

    /// The pgBackRest bundle, checked before anything is created.
    ///
    /// A missing bundle is a host setup problem, and saying so at `apply`
    /// or `backup` time is the difference between one clear message and a
    /// `pg_wal` that quietly stops draining.
    pub(crate) fn pgbackrest_bundle(&self) -> Result<std::path::PathBuf> {
        let bundle = self.layout.pgbackrest_bundle_for_host();
        if !bundle.join("bin/pgbackrest").exists() {
            return Err(Error::PgBackRestMissing(bundle.display().to_string()));
        }
        Ok(bundle)
    }
}

/// A compact timestamp, for naming job containers.
fn now_stamp() -> String {
    chrono::Utc::now().format("%Y%m%dt%H%M%S").to_string()
}
