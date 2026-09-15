//! Major-version upgrades, orchestrated.
//!
//! The shape is the one ADR 05 built the pooler for, with `pg_upgrade` in
//! the middle instead of a container swap: **hold the clients, change the
//! instance underneath, release**. What is new is that the change needs
//! two images at once, so the window is bracketed by work that happens
//! *outside* it — staging the old installation while the cluster is still
//! serving, and analysing and backing up once it is serving again.
//!
//! Everything that can refuse, refuses before the hold is taken. By the
//! time a client waits, the only things left are a container stop, a
//! `pg_upgrade` and a container start — the three steps that have to be
//! fast, and the only three that cannot be checked in advance.

use std::time::{Duration, Instant};

use pgpod_core::{
    ClusterId, ClusterManifest, ProbeReport, StageReport, UPGRADE_SPEC_ENV, UpgradeMethod,
    UpgradeRunReport, UpgradeSpec, container, version_key,
};
use pgpod_runtime::{ContainerSpec, ExecSpec, Mount};

use crate::pooler::PoolerHold;
use crate::{ApplyContext, Error, Pgpod, Result, names};

/// How one upgrade should behave.
#[derive(Debug, Clone)]
pub struct UpgradeOptions {
    /// The image to upgrade *to*. Its PostgreSQL major version is read
    /// from the image rather than from its tag, because a tag is a string
    /// somebody typed.
    pub to_image: String,
    pub method: UpgradeMethod,
    /// `pg_upgrade --jobs`. More helps a cluster with many databases or
    /// many tables; it does not help a single small one.
    pub jobs: u32,
    /// Rehearse: everything up to and including `pg_upgrade --check`, then
    /// put the cluster back the way it was. Still an outage — the old
    /// cluster has to be stopped for `pg_upgrade` to read it — but a short
    /// one that changes nothing.
    pub check: bool,
    /// Run `vacuumdb --analyze-in-stages` afterwards. `pg_upgrade` does
    /// not carry planner statistics across, so the first queries on the
    /// upgraded cluster plan against nothing until this runs.
    pub analyze: bool,
    /// Take a full backup afterwards. A major upgrade gives the cluster a
    /// new system identifier, so the repository's existing backups belong
    /// to a cluster that no longer exists.
    pub backup: bool,
    /// How long to wait for the upgraded instance to accept connections.
    pub wait: Duration,
}

impl Default for UpgradeOptions {
    fn default() -> Self {
        Self {
            to_image: String::new(),
            method: UpgradeMethod::Link,
            jobs: 2,
            check: false,
            analyze: true,
            backup: true,
            wait: Duration::from_secs(300),
        }
    }
}

/// What `upgrade` did.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeReport {
    pub cluster: String,
    pub from_image: String,
    pub to_image: String,
    pub from_version: String,
    pub to_version: String,
    pub method: String,
    /// True when nothing was changed because this was a rehearsal.
    pub checked_only: bool,
    /// How many poolers held clients across the window. **Zero means
    /// every open connection was dropped** — the upgrade still works, it
    /// is simply an outage rather than a pause.
    pub poolers_held: usize,
    /// How long clients were actually held, end to end. This is the
    /// number to compare against the pooler's `maxHold`.
    pub held_ms: u128,
    /// How long `pg_upgrade` itself ran.
    pub upgrade_seconds: u64,
    pub staged_bytes: u64,
    /// Where the pre-upgrade data directory was left. pgpod never removes
    /// it (AGENTS.md principle 4).
    pub old_data_dir: String,
    pub analyzed: bool,
    /// Label of the backup taken afterwards, when one was.
    pub backup_label: Option<String>,
    /// Things the operator has to know and nothing refused over.
    pub warnings: Vec<String>,
}

impl Pgpod {
    /// Upgrade a cluster to a new PostgreSQL major version.
    pub async fn upgrade(
        &self,
        cluster: &ClusterId,
        options: UpgradeOptions,
    ) -> Result<UpgradeReport> {
        let record = self.registry.require_cluster(cluster)?;
        let manifest = record.manifest.clone();
        let instance = cluster.instance(1);
        let mut warnings = Vec::new();

        if manifest.spec.instances != 1 {
            // Upgrading a primary underneath its standbys leaves them
            // replaying WAL from a timeline that no longer exists. The
            // answer is to rebuild them from the upgraded primary, which
            // needs Phase 3's standbys to exist first.
            return Err(Error::Invalid(format!(
                "{cluster} has {} instances, and pgpod can only upgrade a \
                 single-instance cluster today — a standby cannot follow its \
                 primary across a major version",
                manifest.spec.instances
            )));
        }
        if options.to_image.trim() == manifest.spec.image_name.trim() {
            return Err(Error::Invalid(format!(
                "{cluster} already runs {}. A major upgrade needs a different \
                 image; a minor one is `pgpod apply --recreate`.",
                manifest.spec.image_name
            )));
        }

        let agent = self.agent_binary()?;
        let mut target = manifest.clone();
        target.spec.image_name = options.to_image.trim().to_string();

        // ---- everything that can refuse, refuses here ----------------

        self.podman
            .pull_image_if_absent(&target.spec.image_name)
            .await?;

        // The version on disk, read from the instance that is running it.
        // An upgrade needs the cluster up to start with: the version has
        // to be read before anything is stopped, and stopping it is the
        // first thing that happens inside the hold.
        let from_version = self.running_version(&instance).await.map_err(|e| {
            Error::Invalid(format!(
                "{cluster} has to be running to be upgraded — its data \
                 directory's version is read from inside the instance before \
                 anything is changed ({e}). Bring it up with `pgpod apply -f \
                 <manifest>` first."
            ))
        })?;
        let probe: ProbeReport = self
            .upgrade_job(
                &target,
                &instance,
                "upgrade-probe",
                &["upgrade", "probe"],
                None,
                Duration::from_secs(120),
            )
            .await?;

        let (from_key, to_key) = (
            version_key(&from_version).unwrap_or(0),
            version_key(&probe.version).unwrap_or(0),
        );
        if to_key == from_key {
            return Err(Error::Invalid(format!(
                "{} is also PostgreSQL {from_version}. `pgpod upgrade` is for \
                 major versions; for a minor-version image bump use \
                 `pgpod apply --recreate`, which needs no pg_upgrade and no \
                 second copy of the data.",
                target.spec.image_name
            )));
        }
        if to_key < from_key {
            return Err(Error::Invalid(format!(
                "{} carries PostgreSQL {}, which is older than this cluster's \
                 {from_version}. pg_upgrade only goes forwards, and a data \
                 directory cannot be read by an older major version at all.",
                target.spec.image_name, probe.version
            )));
        }

        let poolers = self.registry.poolers_for_cluster(cluster)?;
        if poolers.is_empty() {
            warnings.push(format!(
                "no pooler fronts {cluster}, so every open connection will be \
                 dropped rather than held. `pgpod apply -f pooler.yaml` first \
                 if that matters."
            ));
        }
        for pooler in &poolers {
            let budget = pooler.manifest.spec.pg_doorman.max_hold;
            warnings.push(format!(
                "pooler {} holds clients for at most {} — a client whose query \
                 waits longer than that gets an error rather than a pause, and \
                 the budget is fixed at the pooler's container-create time \
                 (ADR 05 §3)",
                pooler.name,
                budget.render()
            ));
        }
        if manifest.spec.backup.is_enabled() {
            let backups = self.backups(cluster).await.unwrap_or_default();
            if backups.is_empty() {
                warnings.push(format!(
                    "{cluster} archives but has no backup yet, so there is \
                     nothing to restore if this goes wrong. `pgpod backup \
                     {cluster}` first."
                ));
            }
        } else if options.method == UpgradeMethod::Link {
            warnings.push(format!(
                "{cluster} has no backup destination, and --method link leaves \
                 the old cluster unstartable. There is no way back from this \
                 upgrade."
            ));
        }

        // ---- staging: no downtime, and the slow part -----------------

        self.registry.record_event(
            cluster.as_str(),
            Some(1),
            "info",
            &format!(
                "upgrade {from_version} -> {} requested ({} mode); staging the \
                 old installation",
                probe.version,
                options.method.as_str()
            ),
        )?;

        let stage: StageReport = self
            .upgrade_job(
                &manifest,
                &instance,
                "upgrade-stage",
                &["upgrade", "stage"],
                None,
                Duration::from_secs(900),
            )
            .await?;
        if stage.version != from_version {
            return Err(Error::Invalid(format!(
                "the running image carries PostgreSQL {}, but the cluster's \
                 data directory is {from_version}",
                stage.version
            )));
        }

        let spec = UpgradeSpec {
            from_version: from_version.clone(),
            to_version: probe.version.clone(),
            staged_bindir: stage.bindir.clone(),
            staged_lib_dirs: stage.lib_dirs.clone(),
            method: options.method,
            jobs: options.jobs,
            preload_libraries: manifest.spec.postgresql.shared_preload_libraries.clone(),
            check: options.check,
            // The new cluster has to be initdb'd the way the old one was:
            // pg_upgrade compares encoding and locale and refuses a pair
            // that disagrees. These are the settings that created the old
            // cluster, which is the closest thing to the truth pgpod has
            // — and pg_upgrade's own check is the backstop if the cluster
            // on disk was made some other way.
            initdb: manifest.spec.bootstrap.initdb.clone(),
        };
        let spec_env = spec
            .to_env_value()
            .map_err(|e| Error::Invalid(format!("upgrade spec: {e}")))?;

        // ---- the window ---------------------------------------------

        // The phase moves first, and separately from the manifest. Below,
        // `put_cluster(&target, "upgrading")` cannot run any earlier than
        // it does — it swaps the stored *manifest*, which must not happen
        // before the data on disk is the new version. But the phase is not
        // the manifest, and leaving it at `running` for the whole window
        // means a host that reboots mid-upgrade comes back saying the
        // cluster is running while its primary's container has been
        // stopped and removed. Anything reading the phase to decide
        // whether a cluster may be touched needs this fence, and `--check`
        // opens the same window, so it is inside it too.
        self.registry.set_cluster_phase(cluster, "upgrading")?;

        let mut hold = PoolerHold::acquire(self, cluster).await?;
        let held_count = hold.len();
        let held_from = Instant::now();

        let outcome = self
            .upgrade_held(&target, &instance, &spec_env, &options)
            .await;

        let run = match outcome {
            Ok(run) => run,
            Err(e) => {
                // Nothing was swapped — every failure before the rename
                // leaves the old cluster where it was — so the way back
                // is to start it again on the image it was already on.
                self.registry.record_event(
                    cluster.as_str(),
                    Some(1),
                    "error",
                    &format!("upgrade failed, restarting the old instance: {e}"),
                )?;
                if let Err(restart) = self.start_instance(&manifest, &instance, &agent).await {
                    // The window closed with the cluster down, so the
                    // phase must not go back to `running`. `failed` is
                    // what says a human has to look at this before
                    // anything else acts on the cluster.
                    if let Err(p) = self.registry.set_cluster_phase(cluster, "failed") {
                        tracing::warn!("could not record the failed phase for {cluster}: {p}");
                    }
                    hold.release().await;
                    return Err(Error::Invalid(format!(
                        "the upgrade failed ({e}), and so did bringing the old \
                         instance back up ({restart}). The cluster's data is \
                         untouched at {} inside volume {} — no swap had \
                         happened when this failed.",
                        container::PGDATA,
                        instance.volume_name()
                    )));
                }
                // Wait for the old instance to accept connections before
                // handing the held clients back to it. Best-effort: the
                // error worth returning is the upgrade's, not this wait's,
                // but releasing onto a postmaster that is still starting
                // would turn a failed upgrade into dropped connections
                // too.
                let _ = self.wait_ready(&instance, options.wait).await;
                // The old instance is back on its own image and serving,
                // so the cluster is exactly what it was before the window
                // opened — including its phase. Nothing was swapped.
                self.registry.set_cluster_phase(cluster, "running")?;
                hold.release_after_recreate().await?;
                return Err(e);
            }
        };

        if options.check {
            // A rehearsal ends where it started: the same image, the same
            // data, and a measured window.
            self.start_instance(&manifest, &instance, &agent).await?;
            self.wait_ready(&instance, options.wait).await?;
            // Same image, same data, same phase — a rehearsal leaves
            // nothing behind, and that includes `upgrading`.
            self.registry.set_cluster_phase(cluster, "running")?;
            hold.release_after_recreate().await?;
            let held_ms = held_from.elapsed().as_millis();
            self.registry.record_event(
                cluster.as_str(),
                Some(1),
                "info",
                &format!("upgrade check passed in {held_ms}ms; nothing was changed"),
            )?;
            return Ok(UpgradeReport {
                cluster: cluster.to_string(),
                from_image: manifest.spec.image_name.clone(),
                to_image: target.spec.image_name.clone(),
                from_version,
                to_version: probe.version,
                method: options.method.as_str().to_string(),
                checked_only: true,
                poolers_held: held_count,
                held_ms,
                upgrade_seconds: run.seconds,
                staged_bytes: stage.bytes,
                old_data_dir: String::new(),
                analyzed: false,
                backup_label: None,
                warnings,
            });
        }

        // The data on disk is the new version now, so the registry has to
        // say so before anything else can go wrong: a stored manifest
        // naming the old image would have a later `apply` start a
        // PostgreSQL that refuses the data directory it is given.
        self.registry.put_cluster(&target, "upgrading")?;

        self.start_instance(&target, &instance, &agent).await?;
        self.wait_ready(&instance, options.wait).await?;

        // Before the hold is released, because archiving resumes with the
        // instance: pgBackRest refuses a segment from a cluster whose
        // system identifier does not match its stanza, and every one of
        // those is a failed `archive_command` retrying against a
        // repository that will never accept it.
        if manifest.spec.backup.is_enabled()
            && let Err(e) = self.stanza_upgrade(cluster).await
        {
            warnings.push(format!(
                "pgbackrest stanza-upgrade failed, so WAL from the upgraded \
                 cluster will not be archived until it is run: {e}"
            ));
        }

        hold.release_after_recreate().await?;
        let held_ms = held_from.elapsed().as_millis();

        self.registry.record_event(
            cluster.as_str(),
            Some(1),
            "info",
            &format!(
                "upgraded {from_version} -> {} in {held_ms}ms held ({}s in \
                 pg_upgrade); the old data directory is kept at {}",
                probe.version, run.seconds, run.old_data_dir
            ),
        )?;

        // ---- after the window ---------------------------------------

        let analyzed = if options.analyze {
            match self.analyze_in_stages(&instance).await {
                Ok(()) => true,
                Err(e) => {
                    warnings.push(format!(
                        "the cluster is up, but `vacuumdb --analyze-in-stages` \
                         failed: {e}. Until it runs, every query plans against \
                         no statistics."
                    ));
                    false
                }
            }
        } else {
            warnings.push(
                "planner statistics were not rebuilt — run `vacuumdb --all \
                 --analyze-in-stages` before this cluster takes real traffic"
                    .to_string(),
            );
            false
        };

        let mut backup_label = None;
        if options.backup {
            if manifest.spec.backup.is_enabled() {
                match self.backup(cluster, Duration::from_secs(3600)).await {
                    Ok(report) => backup_label = report.label,
                    Err(e) => warnings.push(format!(
                        "the post-upgrade backup failed: {e}. The repository's \
                         older backups belong to the pre-upgrade cluster, so \
                         there is no recovery point for the new one until a \
                         backup succeeds."
                    )),
                }
            } else {
                warnings.push(format!(
                    "{cluster} has no backup destination, so no post-upgrade \
                     backup was taken"
                ));
            }
        }

        self.registry.set_cluster_phase(cluster, "running")?;

        Ok(UpgradeReport {
            cluster: cluster.to_string(),
            from_image: manifest.spec.image_name.clone(),
            to_image: target.spec.image_name.clone(),
            from_version,
            to_version: probe.version,
            method: options.method.as_str().to_string(),
            checked_only: false,
            poolers_held: held_count,
            held_ms,
            upgrade_seconds: run.seconds,
            staged_bytes: stage.bytes,
            old_data_dir: run.old_data_dir,
            analyzed,
            backup_label,
            warnings,
        })
    }

    /// Refuse an `apply` whose image cannot read the data on disk.
    ///
    /// Skipped for the steady state — an instance already running the very
    /// image this manifest names, which is proof enough that the two are
    /// compatible — and run for everything else: a changed image, a
    /// stopped instance, or a volume with no container at all after a
    /// `delete`. Both outcomes it refuses are failures `apply` would
    /// otherwise produce with PostgreSQL's words rather than pgpod's:
    ///
    /// * **Newer image, older data.** Recreating the container starts a
    ///   PostgreSQL that says `database files are incompatible with
    ///   server` and exits, leaving an instance that never becomes ready.
    ///   The data is untouched, which is the only good part.
    /// * **Older image, newer data.** The same failure in reverse, and
    ///   the likely cause is applying a `cluster.yaml` that was not
    ///   updated after `pgpod upgrade` rewrote what pgpod stores.
    ///
    /// A same-major change — a minor-version bump, or a different base
    /// image of the same PostgreSQL — is *not* refused here. It is a real
    /// change that needs the container recreated, and the divergence
    /// machinery handles it: refused by default, applied by `--recreate`
    /// with the pooler holding.
    pub(crate) async fn refuse_incompatible_image(
        &self,
        cluster: &ClusterId,
        desired: &ClusterManifest,
        desired_image_id: Option<&str>,
    ) -> Result<()> {
        let instance = cluster.instance(1);
        // Nothing on disk yet — a cluster that has never been
        // bootstrapped can have any image. Checked through podman rather
        // than the registry, because it is the volume that constrains
        // this, not what pgpod remembers.
        if self.podman.volume(&instance.volume_name()).await?.is_none() {
            return Ok(());
        }

        // Already running this exact image. A PostgreSQL that is serving
        // queries out of this data directory has settled the question,
        // and probing on every apply of an unchanged manifest would put a
        // container start in the middle of the commonest command there
        // is.
        if let Some(probe) = self
            .podman
            .container(instance.container_name())
            .probe()
            .await?
            && probe.running
            && probe.image_id.is_some()
            && probe.image_id.as_deref() == desired_image_id
        {
            return Ok(());
        }

        // One job container answers both halves: the image's version, and
        // the version of the cluster in the volume it mounts.
        let probe: ProbeReport = self
            .upgrade_job(
                desired,
                &instance,
                "image-probe",
                &["upgrade", "probe"],
                None,
                Duration::from_secs(300),
            )
            .await?;

        let Some(on_disk) = probe.data_version.as_deref() else {
            // A volume with no data directory in it. Whatever the image
            // is, there is nothing for it to be incompatible with.
            return Ok(());
        };
        let (disk_key, image_key) = (
            version_key(on_disk).unwrap_or(0),
            version_key(&probe.version).unwrap_or(0),
        );
        if disk_key == image_key {
            return Ok(());
        }

        // Both messages are written from the **data directory** outwards,
        // and neither quotes the stored manifest. That record says what
        // was last asked for, not what worked — an apply that already
        // failed on this image recorded it — so telling an operator "the
        // cluster is still on X" from it can be a lie at exactly the
        // moment they need the truth.
        let to = &desired.spec.image_name;
        if image_key > disk_key {
            return Err(Error::Invalid(format!(
                "{cluster} holds a PostgreSQL {on_disk} data directory, and \
                 {to} is PostgreSQL {}. `apply` cannot make that change: it \
                 would start the new server on the old data, which fails with \
                 \"database files are incompatible with server\".\n\n\
                 A major version needs pg_upgrade, which is a different \
                 operation with a different window:\n\n  \
                 pgpod upgrade {cluster} --to-image {to}\n\n\
                 Nothing has been changed — the data directory is still \
                 PostgreSQL {on_disk}.",
                probe.version
            )));
        }
        Err(Error::Invalid(format!(
            "{cluster} holds a PostgreSQL {on_disk} data directory, and {to} \
             is PostgreSQL {} — older. PostgreSQL cannot read a data \
             directory written by a later major version, so this apply would \
             leave an instance that never starts.\n\n\
             If this cluster has been through `pgpod upgrade`, this file is \
             the stale half: set `imageName` to a PostgreSQL {on_disk} image \
             to match the data.\n\n\
             Nothing has been changed.",
            probe.version
        )))
    }

    /// The part that happens while clients are held: stop, upgrade.
    ///
    /// Deliberately does **not** start anything. The caller decides which
    /// image comes back up, and on the failure path that is the old one.
    async fn upgrade_held(
        &self,
        target: &ClusterManifest,
        instance: &pgpod_core::InstanceId,
        spec_env: &str,
        options: &UpgradeOptions,
    ) -> Result<UpgradeRunReport> {
        let cluster = instance.cluster();
        self.registry.record_event(
            cluster.as_str(),
            Some(instance.ordinal()),
            "info",
            "stopping the instance for pg_upgrade",
        )?;

        // A *clean* stop, which is what pg_upgrade requires: the agent
        // turns SIGTERM into PostgreSQL's fast shutdown, and the timeout
        // is generous because a SIGKILL here would leave a cluster
        // needing recovery that pg_upgrade then refuses to read.
        let handle = self.podman.container(instance.container_name());
        if handle.probe().await?.is_some() {
            handle.stop(Duration::from_secs(120)).await?;
            handle.remove(true).await?;
        }

        self.registry.record_event(
            cluster.as_str(),
            Some(instance.ordinal()),
            "info",
            if options.check {
                "running pg_upgrade --check"
            } else {
                "running pg_upgrade"
            },
        )?;

        // In the *new* image, with the instance stopped. The old
        // installation is already in the volume.
        self.upgrade_job(
            target,
            instance,
            "upgrade-run",
            &["upgrade", "run"],
            Some(spec_env),
            // pg_upgrade in link mode is seconds; in copy mode it is
            // proportional to the data, and an operator who chose copy
            // chose to wait.
            match options.method {
                UpgradeMethod::Link => Duration::from_secs(3600),
                UpgradeMethod::Copy => Duration::from_secs(24 * 3600),
            },
        )
        .await
    }

    /// Create and start the instance container for `manifest`.
    ///
    /// Used for the upgraded instance and, on the failure path, to put
    /// the old one back. It goes through the same
    /// [`Pgpod::container_spec`] every apply uses, so the container an
    /// upgrade leaves behind is indistinguishable from one `apply` would
    /// have made — otherwise the next `apply` would report divergence
    /// that is really a difference in how two code paths built the same
    /// spec.
    async fn start_instance(
        &self,
        manifest: &ClusterManifest,
        instance: &pgpod_core::InstanceId,
        agent: &std::path::Path,
    ) -> Result<()> {
        let cluster = instance.cluster().clone();
        let record = self
            .registry
            .instance(&cluster, instance.ordinal())?
            .ok_or_else(|| Error::Invalid(format!("{cluster} has no instance row")))?;

        let network = self
            .podman
            .ensure_network(&cluster.network_name(), &names::cluster_labels(&cluster))
            .await?;
        let secrets = crate::SecretNames::for_cluster(&cluster);
        let ctx = ApplyContext {
            bootstrap: pgpod_core::Bootstrap::Initdb(manifest.spec.bootstrap.initdb.clone()),
            network_cidr: network.subnet.clone(),
            secrets,
            agent,
            recreate: false,
            // Not a convergence: this builds the container the caller
            // named, so there is nothing to compare it against.
            desired_image_id: None,
        };

        let handle = self.podman.container(instance.container_name());
        if handle.probe().await?.is_some() {
            let _ = handle.stop(Duration::from_secs(60)).await;
            handle.remove(true).await?;
        }
        let spec = self.container_spec(manifest, instance, &ctx, record.host_port)?;
        let container = self.podman.create_container(&spec).await?;
        container.start().await?;

        let mut rec = record;
        rec.container_id = Some(container.id().to_string());
        rec.phase = pgpod_core::InstancePhase::Bootstrapping;
        self.registry.put_instance(&rec)?;
        Ok(())
    }

    /// Teach the pgBackRest repository about the new major version.
    ///
    /// A major upgrade gives the cluster a new system identifier and
    /// catalog version, and pgBackRest checks both on every
    /// `archive-push`. Without this the first segment after the upgrade
    /// is rejected, `archive_command` starts failing, and `pg_wal` grows
    /// — correct back-pressure for a repository that really is wrong, and
    /// entirely avoidable here.
    pub(crate) async fn stanza_upgrade(&self, cluster: &ClusterId) -> Result<()> {
        let record = self.registry.require_cluster(cluster)?;
        let name = format!("pgpod-{cluster}-stanza-upgrade");
        let out = self
            .run_pgbackrest(
                &record.manifest,
                cluster,
                &name,
                &["stanza-upgrade"],
                Duration::from_secs(600),
            )
            .await?;
        out.map(|_| ())
    }

    /// Rebuild planner statistics, which `pg_upgrade` does not carry over.
    ///
    /// `--analyze-in-stages` rather than a plain analyze: it makes three
    /// passes, the first with a tiny sample, so the cluster has *some*
    /// statistics in seconds rather than none for minutes. That ordering
    /// is the whole reason it exists, and it matters most exactly here,
    /// where traffic resumes the moment the hold is released.
    async fn analyze_in_stages(&self, instance: &pgpod_core::InstanceId) -> Result<()> {
        let container = self.running_container(instance).await?;
        let out = container
            .exec(&ExecSpec::new([
                "vacuumdb",
                "--all",
                "--analyze-in-stages",
                "-h",
                container::SOCKET_DIR,
                "-U",
                "postgres",
            ]))
            .await?;
        if !out.success() {
            return Err(Error::Invalid(format!(
                "vacuumdb failed: {}{}",
                out.stdout.trim(),
                out.stderr.trim()
            )));
        }
        Ok(())
    }

    /// The major version of the cluster on disk, read from the instance
    /// that is running it.
    ///
    /// `PG_VERSION` rather than `SELECT version()`: it is what
    /// `pg_upgrade` compares, it needs no connection, and it is still
    /// readable on a cluster too broken to answer a query.
    async fn running_version(&self, instance: &pgpod_core::InstanceId) -> Result<String> {
        let container = self.running_container(instance).await?;
        let out = container
            .exec(&ExecSpec::new([
                "cat",
                &format!("{}/PG_VERSION", container::PGDATA),
            ]))
            .await?;
        if !out.success() {
            return Err(Error::Invalid(format!(
                "could not read {}/PG_VERSION from {instance}: {}",
                container::PGDATA,
                out.stderr.trim()
            )));
        }
        pgpod_core::major_label(&out.stdout).ok_or_else(|| {
            Error::Invalid(format!(
                "{instance} reports a data directory version this pgpod does \
                 not understand: {:?}",
                out.stdout.trim()
            ))
        })
    }

    /// Run one upgrade step in a throwaway container and parse its report.
    ///
    /// Which image it runs in is the argument that matters: `stage` goes
    /// in the old one and the other two in the new one, and passing the
    /// wrong manifest here would stage the image that is already running.
    async fn upgrade_job<T: serde::de::DeserializeOwned>(
        &self,
        manifest: &ClusterManifest,
        instance: &pgpod_core::InstanceId,
        job: &str,
        argv: &[&str],
        spec_env: Option<&str>,
        wait: Duration,
    ) -> Result<T> {
        let agent = self.agent_binary()?;
        let name = instance.job_container_name(job);
        // A name left by a previous attempt would make create fail with
        // "name already in use", which says nothing about what went wrong
        // the first time.
        let stale = self.podman.container(&name);
        if stale.probe().await?.is_some() {
            let _ = stale.stop(Duration::from_secs(10)).await;
            let _ = stale.remove(true).await;
        }

        let mut argv_full = vec![container::AGENT_BIN.to_string()];
        argv_full.extend(argv.iter().map(|a| a.to_string()));

        let mut spec = ContainerSpec::hardened(&manifest.spec.image_name)
            .name(&name)
            .user(format!(
                "{}:{}",
                manifest.spec.postgres_uid, manifest.spec.postgres_gid
            ))
            .entrypoint(argv_full)
            // The instance's volume is the whole working surface: PGDATA
            // to upgrade, the staged installation beside it, and the
            // socket directory the two servers pg_upgrade starts will
            // talk over.
            .mount(Mount::Volume {
                name: instance.volume_name(),
                target: container::VOLUME_MOUNT.to_string(),
                chown: false,
            })
            .mount(Mount::BindReadOnly {
                source: agent.display().to_string(),
                target: container::AGENT_BIN.to_string(),
            })
            // This container's stdout carries a report the control plane
            // parses, so it must not go to the host's journal — where
            // journald's 48 KiB LineMax would be free to truncate it.
            // Same reasoning as the pgBackRest job.
            .log_driver("k8s-file")
            .label(crate::LABEL_CLUSTER, instance.cluster().as_str())
            .label("pgpod.job", job);

        if let Some(env) = spec_env {
            spec = spec.env(UPGRADE_SPEC_ENV, env);
        }

        let container = self.podman.create_container(&spec).await?;
        container.start().await?;
        let code = tokio::time::timeout(wait, container.wait_for_exit())
            .await
            .map_err(|_| Error::NotReady {
                instance: instance.to_string(),
                seconds: wait.as_secs(),
                detail: format!("the {job} job is still running"),
            })??;

        // Read before removing, or the reason for a failure goes with it.
        let logs = container
            .logs_string()
            .await
            .unwrap_or_else(|e| format!("<logs unavailable: {e}>"));
        let _ = container.remove(true).await;

        if code != 0 {
            return Err(Error::Upgrade {
                step: job.to_string(),
                detail: logs,
            });
        }
        pgpod_core::parse_report(&logs).map_err(|e| Error::Upgrade {
            step: job.to_string(),
            detail: format!("{e}\n\n{logs}"),
        })
    }
}
