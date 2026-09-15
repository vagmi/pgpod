//! Boot recovery: putting back what was running.
//!
//! Every pgpod container is created with `RestartPolicy::No`, because the
//! reconciler owns restarts and podman's own policy would race it — it
//! would cheerfully restart an instance that `pgpod promote` had
//! deliberately fenced. The cost of that decision is that a host reboot
//! leaves every cluster down until something starts it. This module is
//! that something.
//!
//! [`Pgpod::resume`] **puts back what was running and nothing else.** It
//! creates no containers, pulls no images, bootstraps nothing, and
//! allocates no ports. Those are all things `apply` does with an operator
//! watching; resume runs at boot with nobody watching at all, so every
//! case it is not certain about is reported rather than guessed at.
//!
//! The narrowness is what makes it safe to run unattended, and it is only
//! possible because `podman start` on a stopped container genuinely
//! restores everything the instance needs — verified on the deployment
//! target across real reboots (Ubuntu 26.04 / podman 5.7.0):
//!
//! * mounted secrets are re-materialised into the tmpfs at `/run/secrets`,
//!   even though the container's rundir under `XDG_RUNTIME_DIR` is wiped;
//! * `inspect` still reports the spec environment, labels and port
//!   bindings that were fixed at container-create time;
//! * the agent finds PGDATA populated and skips bootstrap;
//! * the published host port is re-bound to the same number.

use std::time::Duration;

use pgpod_core::{ClusterPhase, InstancePhase, PathLayout};
use pgpod_registry::InstanceRecord;
use pgpod_runtime::ContainerProbe;

use crate::{Error, Pgpod, Result};

/// How long to wait for a resumed instance to accept connections.
///
/// Longer than `apply`'s default, and deliberately so. An instance that
/// was killed by a reboot rather than shut down cleanly replays WAL on
/// startup, and how long that takes is a function of how much was in
/// flight — not of anything pgpod can see beforehand. The wait costs
/// nothing when it is not needed, and the alternative is a boot that
/// reports failure for a cluster that was going to come up fine.
pub const DEFAULT_RESUME_WAIT: Duration = Duration::from_secs(600);

/// What resume did about one container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResumeAction {
    /// Already running when resume looked. Nothing was done to it.
    Adopted,
    /// Stopped, and started again. The ordinary boot path.
    Started,
    /// Left alone deliberately, because of a phase that forbids it.
    Skipped,
    /// The registry names a container podman has never heard of. Resume
    /// does **not** create it: that means an image pull and possibly a
    /// bootstrap, which is `apply`'s job.
    Missing,
    /// Starting it was attempted and did not work.
    Failed,
}

impl ResumeAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Adopted => "adopted",
            Self::Started => "started",
            Self::Skipped => "skipped",
            Self::Missing => "missing",
            Self::Failed => "failed",
        }
    }

    /// Whether this outcome is one an operator has to do something about.
    fn needs_attention(&self) -> bool {
        matches!(self, Self::Missing | Self::Failed)
    }
}

impl std::fmt::Display for ResumeAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What resume decided before it acted.
///
/// Separate from [`ResumeAction`] because a decision is made from stored
/// state alone and can therefore be tested exhaustively with no podman in
/// sight, while the action is what actually happened afterwards —
/// `Decision::Start` becomes `ResumeAction::Failed` if the start fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Running already.
    Adopt,
    /// Stopped and eligible; start it.
    Start,
    /// Do not touch it, for this reason.
    Skip(&'static str),
    /// Podman does not know this container.
    Missing,
}

/// The whole of boot recovery's judgement, as a pure function.
///
/// Takes stored state and a probe rather than reaching for either, so
/// every combination is reachable from a unit test. The order of the
/// checks is the substance: the cluster's phase gates everything, because
/// a cluster caught mid-`apply` or mid-`upgrade` must not have *any* of
/// its instances touched; then the instance's own phase, which is where
/// fencing lives; and only then what podman actually reports.
pub(crate) fn decide(
    cluster: ClusterPhase,
    instance: InstancePhase,
    probe: Option<&ContainerProbe>,
) -> Decision {
    if let Some(why) = cluster.skip_reason() {
        return Decision::Skip(why);
    }
    if instance.blocks_restart() {
        return Decision::Skip(why_blocked(instance));
    }
    match probe {
        None => Decision::Missing,
        Some(p) if p.running => Decision::Adopt,
        Some(_) => Decision::Start,
    }
}

/// Why an instance phase forbids a restart, phrased for an operator.
fn why_blocked(phase: InstancePhase) -> &'static str {
    match phase {
        // The split-brain guard. Restarting a fenced old primary during a
        // failover is the one mistake this whole module must never make.
        InstancePhase::Fenced => {
            "fenced during a failover — only an explicit rejoin or rebuild clears this"
        }
        InstancePhase::NeedsRebuild => "pg_rewind failed; it needs `pgpod rebuild`",
        InstancePhase::Stopped => {
            "deliberately stopped — `pgpod apply -f <manifest>` brings it back"
        }
        InstancePhase::Terminated => "terminated",
        other => {
            debug_assert!(
                !other.blocks_restart(),
                "{other} blocks restart with no reason given"
            );
            "not in a restartable phase"
        }
    }
}

/// Instances in the order boot recovery should start them.
///
/// Ordinal 1 — the primary — first, then the standbys in order. A standby
/// retries its upstream on its own, so this is not a correctness
/// requirement; it is the difference between a quiet boot and a journal
/// full of connection failures that resolve themselves.
fn resume_order(mut rows: Vec<InstanceRecord>) -> Vec<InstanceRecord> {
    rows.sort_by_key(|r| r.ordinal);
    rows
}

/// Whether a recorded host port can still be bound.
///
/// Worth checking before a start, and boot is exactly when it can fail:
/// every port pgpod recorded came from binding `127.0.0.1:0`, so it sits
/// inside the ephemeral range, and anything on the host that made an
/// outbound connection while pgpod was not running may be holding one.
/// Podman's own failure here is a rootlessport error that names neither
/// the instance nor the remedy.
///
/// Only meaningful before starting a *stopped* container. A running one is
/// holding its own port, and this would report it as taken.
fn port_available(port: u16) -> std::result::Result<(), String> {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(e) => Err(format!(
            "host port {port} is held by another process ({e}); the container \
             was not started. Free the port and run `pgpod daemon --once`, or \
             re-apply the manifest to move it."
        )),
    }
}

/// What resume did about one container, for the report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UnitResume {
    /// The instance id or pooler name, as an operator would type it.
    pub name: String,
    pub action: ResumeAction,
    /// Why, when the action alone does not say.
    pub detail: Option<String>,
}

/// What resume did about one cluster.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClusterResume {
    pub cluster: String,
    pub phase: String,
    pub instances: Vec<UnitResume>,
    /// Set when the cluster's own phase meant none of it was touched.
    pub skipped: Option<String>,
}

/// What one whole boot recovery did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResumeReport {
    pub clusters: Vec<ClusterResume>,
    pub poolers: Vec<UnitResume>,
}

impl ResumeReport {
    fn units(&self) -> impl Iterator<Item = &UnitResume> {
        self.clusters
            .iter()
            .flat_map(|c| c.instances.iter())
            .chain(self.poolers.iter())
    }

    pub fn counts(&self) -> ResumeCounts {
        let mut c = ResumeCounts::default();
        for u in self.units() {
            match u.action {
                ResumeAction::Adopted => c.adopted += 1,
                ResumeAction::Started => c.started += 1,
                ResumeAction::Skipped => c.skipped += 1,
                ResumeAction::Missing => c.missing += 1,
                ResumeAction::Failed => c.failed += 1,
            }
        }
        c.skipped += self.clusters.iter().filter(|c| c.skipped.is_some()).count();
        c
    }

    /// Whether anything needs a human.
    ///
    /// Deliberately **not** the daemon's exit code. A missing container is
    /// a problem for an operator to look at, not a reason for systemd to
    /// restart the daemon every five seconds re-discovering it.
    pub fn needs_attention(&self) -> bool {
        self.units().any(|u| u.action.needs_attention())
    }
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct ResumeCounts {
    pub adopted: usize,
    pub started: usize,
    pub skipped: usize,
    pub missing: usize,
    pub failed: usize,
}

impl std::fmt::Display for ResumeCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} adopted, {} started, {} skipped, {} missing, {} failed",
            self.adopted, self.started, self.skipped, self.missing, self.failed
        )
    }
}

/// How one `resume` should behave.
#[derive(Debug, Clone, Copy)]
pub struct ResumeOptions {
    /// How long to wait for each started instance to accept connections.
    pub wait: Duration,
}

impl Default for ResumeOptions {
    fn default() -> Self {
        Self {
            wait: DEFAULT_RESUME_WAIT,
        }
    }
}

impl Pgpod {
    /// Bring back the clusters and poolers that were running.
    ///
    /// Idempotent: a second call adopts everything the first started, so
    /// it is safe to run by hand after the daemon has already done it.
    ///
    /// Errors only for the things that make the whole operation
    /// impossible — an unreadable registry, an unreachable podman. A
    /// single instance that will not start is recorded in the report and
    /// the rest of the work continues, because one broken cluster must not
    /// keep every other cluster on the host down.
    pub async fn resume(&self) -> Result<ResumeReport> {
        self.resume_with(ResumeOptions::default()).await
    }

    /// `resume`, with the readiness budget chosen by the caller.
    pub async fn resume_with(&self, options: ResumeOptions) -> Result<ResumeReport> {
        let mut clusters = Vec::new();
        for record in self.registry.list_clusters()? {
            clusters.push(
                self.resume_cluster(&record.name, &record.phase, options)
                    .await?,
            );
        }

        // Poolers last. pg_doorman resolves each backend's host per
        // connect rather than at startup, so it tolerates a down cluster
        // — but starting it after the instances avoids a window where
        // every client connection through it fails.
        let mut poolers = Vec::new();
        for record in self.registry.list_poolers()? {
            let mut outcome = self
                .resume_container(
                    &record.name,
                    &record.container_name,
                    ClusterPhase::parse_lenient(&record.phase),
                    InstancePhase::Running,
                    Some(record.host_port),
                )
                .await;

            // A started pooler is checked, where a started instance is
            // only waited for. The difference is that `podman start`
            // returning success is not evidence a pooler came back: it
            // renders its whole configuration on every start, so it can
            // exit a second later with the container having looked healthy
            // in between. Reporting that as `started` would be a boot
            // report that says everything is fine about a dead pooler.
            //
            // The concrete case: a pooler container created before tmpfs
            // mounts carried `mode=1777` cannot write `/pooler` after a
            // restart, because podman mounts it `0755` root-owned on every
            // start but `1777` at create. Its mount options are fixed at
            // create time, so re-applying the pooler is the way out — and
            // this is what tells an operator to.
            if outcome.action == ResumeAction::Started
                && let Ok(id) = pgpod_core::PoolerId::new(record.name.clone())
                && let Err(e) = self.wait_pooler_ready(&id, options.wait).await
            {
                outcome.action = ResumeAction::Failed;
                outcome.detail = Some(format!(
                    "started, but it did not come up: {e}\n\nA pooler renders its \
                     config on every start and keeps nothing durable, so \
                     `pgpod apply -f <pooler manifest>` recreates it safely."
                ));
            }

            poolers.push(outcome);
        }

        Ok(ResumeReport { clusters, poolers })
    }

    async fn resume_cluster(
        &self,
        name: &str,
        stored_phase: &str,
        options: ResumeOptions,
    ) -> Result<ClusterResume> {
        let phase = ClusterPhase::parse_lenient(stored_phase);
        let cluster = match pgpod_core::ClusterId::new(name.to_string()) {
            Ok(c) => c,
            // A name the registry holds but `ClusterId` rejects cannot be
            // acted on at all — it is reported rather than silently
            // dropped, because a cluster missing from a boot report looks
            // exactly like a cluster that came up fine.
            Err(e) => {
                return Ok(ClusterResume {
                    cluster: name.to_string(),
                    phase: phase.to_string(),
                    instances: Vec::new(),
                    skipped: Some(format!("unusable cluster name in the registry: {e}")),
                });
            }
        };

        if let Some(why) = phase.skip_reason() {
            self.record_resume_event(
                name,
                if phase == ClusterPhase::Stopped {
                    "info"
                } else {
                    "warn"
                },
                &format!("boot recovery skipped this cluster: {why}"),
            );
            return Ok(ClusterResume {
                cluster: name.to_string(),
                phase: phase.to_string(),
                instances: Vec::new(),
                skipped: Some(why.to_string()),
            });
        }

        let mut instances = Vec::new();
        for row in resume_order(self.registry.instances(&cluster)?) {
            let id = cluster.instance(row.ordinal);
            let outcome = self
                .resume_container(
                    &id.to_string(),
                    &row.container_name,
                    phase,
                    row.phase,
                    Some(row.host_port),
                )
                .await;

            // Wait for the primary before starting the standbys, so they
            // find an upstream rather than retrying against nothing.
            // Best effort: an instance that is slow to replay must not
            // stop the rest of the cluster from being started.
            if outcome.action == ResumeAction::Started
                && let Err(e) = self.wait_ready(&id, options.wait).await
            {
                tracing::warn!("{id} was started but did not become ready: {e}");
                self.record_resume_event(name, "warn", &format!("{id} started but not ready: {e}"));
            }

            instances.push(outcome);
        }

        let counts = instances.iter().fold(ResumeCounts::default(), |mut c, u| {
            match u.action {
                ResumeAction::Adopted => c.adopted += 1,
                ResumeAction::Started => c.started += 1,
                ResumeAction::Skipped => c.skipped += 1,
                ResumeAction::Missing => c.missing += 1,
                ResumeAction::Failed => c.failed += 1,
            }
            c
        });
        // One summary event per cluster per boot, not one per instance per
        // decision: `events` is append-only with no retention, and a
        // daemon that restarts would otherwise grow the table forever.
        if !instances.is_empty() {
            let level = if instances.iter().any(|u| u.action.needs_attention()) {
                "warn"
            } else {
                "info"
            };
            self.record_resume_event(name, level, &format!("boot recovery: {counts}"));
        }

        Ok(ClusterResume {
            cluster: name.to_string(),
            phase: phase.to_string(),
            instances,
            skipped: None,
        })
    }

    /// Decide and act on one container, instance or pooler alike.
    ///
    /// Never returns `Err`: everything that can go wrong for a single
    /// container is an outcome to report, not a reason to abandon the rest
    /// of the host.
    async fn resume_container(
        &self,
        name: &str,
        container_name: &str,
        cluster_phase: ClusterPhase,
        unit_phase: InstancePhase,
        host_port: Option<u16>,
    ) -> UnitResume {
        let handle = self.podman.container(container_name);
        let probe = match handle.probe().await {
            Ok(p) => p,
            Err(e) => {
                return UnitResume {
                    name: name.to_string(),
                    action: ResumeAction::Failed,
                    detail: Some(format!("could not ask podman about it: {e}")),
                };
            }
        };

        match decide(cluster_phase, unit_phase, probe.as_ref()) {
            Decision::Adopt => UnitResume {
                name: name.to_string(),
                action: ResumeAction::Adopted,
                detail: None,
            },
            Decision::Skip(why) => UnitResume {
                name: name.to_string(),
                action: ResumeAction::Skipped,
                detail: Some(why.to_string()),
            },
            Decision::Missing => UnitResume {
                name: name.to_string(),
                action: ResumeAction::Missing,
                detail: Some(format!(
                    "podman has no container {container_name}. Its volume may \
                     still hold the data — `pgpod apply -f <manifest>` recreates \
                     the container around it."
                )),
            },
            Decision::Start => {
                // Checked only on this branch: a container that is already
                // running is holding its own port, and pre-flighting it
                // would report the instance as blocking itself.
                if let Some(port) = host_port
                    && let Err(detail) = port_available(port)
                {
                    return UnitResume {
                        name: name.to_string(),
                        action: ResumeAction::Failed,
                        detail: Some(detail),
                    };
                }
                match handle.start().await {
                    Ok(()) => UnitResume {
                        name: name.to_string(),
                        action: ResumeAction::Started,
                        detail: None,
                    },
                    Err(e) => UnitResume {
                        name: name.to_string(),
                        action: ResumeAction::Failed,
                        detail: Some(format!("start failed: {e}")),
                    },
                }
            }
        }
    }

    /// Record a boot-recovery event, never letting the bookkeeping fail
    /// the recovery it is describing.
    fn record_resume_event(&self, cluster: &str, level: &str, detail: &str) {
        if let Err(e) = self.registry.record_event(cluster, None, level, detail) {
            tracing::warn!("could not record a boot recovery event for {cluster}: {e}");
        }
    }
}

/// Every host port the registry has recorded, instances and poolers alike.
///
/// Lives here rather than in the CLI so `doctor` does not have to open the
/// registry itself (`AGENTS.md` principle 7). Opens the database, so
/// callers that only want to look should check it exists first —
/// `Registry::open` creates and migrates one that does not.
pub fn recorded_host_ports(layout: &PathLayout) -> Result<Vec<u16>> {
    let registry = pgpod_registry::Registry::open(&layout.registry_db())?;
    let mut ports: Vec<u16> = Vec::new();
    for cluster in registry.list_clusters()? {
        if let Ok(id) = pgpod_core::ClusterId::new(cluster.name) {
            ports.extend(registry.instances(&id)?.iter().map(|i| i.host_port));
        }
    }
    ports.extend(registry.list_poolers()?.iter().map(|p| p.host_port));
    Ok(ports)
}

/// The exclusive right to act on this host's containers.
///
/// Held for the daemon's whole life so that a hand-run `pgpod daemon
/// --once` cannot resume the same clusters as the unit at the same time,
/// each starting containers the other is still deciding about.
///
/// It lives in `pgpod-control` rather than in the CLI because what it
/// protects — the registry and podman — is shared by any embedder, not
/// just by `pgpod` the command (`AGENTS.md` principle 7).
///
/// The lock file lives in the XDG runtime directory on purpose: that is
/// wiped at boot, so a lock can never be inherited from a previous life of
/// the machine. `flock` is released by the kernel when the process exits,
/// including when it is killed, so there is no stale-lock case to recover
/// from either.
#[derive(Debug)]
pub struct DaemonLock {
    /// Dropping this releases the lock and closes the file.
    _flock: nix::fcntl::Flock<std::fs::File>,
    path: std::path::PathBuf,
}

impl DaemonLock {
    /// The lock file's path for a given layout.
    pub fn path(layout: &PathLayout) -> std::path::PathBuf {
        layout.runtime_dir().join("pgpod-daemon.lock")
    }

    /// Take the lock, or say who has it.
    pub fn acquire(layout: &PathLayout) -> Result<Self> {
        let path = Self::path(layout);
        std::fs::create_dir_all(layout.runtime_dir()).map_err(|e| {
            Error::Invalid(format!(
                "could not create {}: {e}",
                layout.runtime_dir().display()
            ))
        })?;

        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::Invalid(format!("could not open {}: {e}", path.display())))?;

        match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
            Ok(flock) => Ok(Self {
                _flock: flock,
                path,
            }),
            Err((_, errno)) => Err(Error::Invalid(format!(
                "another pgpod daemon is already running on this host, holding \
                 the lock on {} ({errno}).\n\n\
                 Boot recovery is that daemon's job and it has already done \
                 it — `pgpod status` shows the result, and `journalctl --user \
                 -u pgpod-daemon.service` shows what it did at boot. \
                 `pgpod daemon --once` is for hosts where no daemon is \
                 running; to take over from the one that is:\n  \
                 systemctl --user stop pgpod-daemon.service",
                path.display()
            ))),
        }
    }

    pub fn path_held(&self) -> &std::path::Path {
        &self.path
    }
}

/// Whether a daemon is running, as observed from the lock rather than
/// inferred from systemd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    /// The lock is held: a daemon is up.
    Running,
    /// The lock file exists and is free: a daemon ran and has exited.
    NotRunning,
    /// No lock file at all: no daemon has run since this host booted.
    NeverRan,
    /// The lock file could not be examined.
    Unknown(String),
}

impl DaemonState {
    /// Look at the lock without creating it.
    ///
    /// Deliberately opens without `O_CREAT`, so merely asking the question
    /// cannot leave a lock file behind and turn `NeverRan` into
    /// `NotRunning` for the next caller.
    pub fn probe(layout: &PathLayout) -> Self {
        let path = DaemonLock::path(layout);
        let file = match std::fs::OpenOptions::new().read(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::NeverRan,
            Err(e) => return Self::Unknown(e.to_string()),
        };
        match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
            // Taking it means nobody had it. Dropping it immediately
            // releases it again, which is the point of the probe.
            Ok(_) => Self::NotRunning,
            Err(_) => Self::Running,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(running: bool) -> ContainerProbe {
        ContainerProbe {
            id: "deadbeef".into(),
            name: Some("pgpod-c-1".into()),
            running,
            status: Some(if running { "running" } else { "exited" }.into()),
            exit_code: Some(0),
            env: Vec::new(),
            image_id: None,
            image_name: None,
        }
    }

    const BLOCKING: [InstancePhase; 4] = [
        InstancePhase::Fenced,
        InstancePhase::NeedsRebuild,
        InstancePhase::Stopped,
        InstancePhase::Terminated,
    ];

    const RESTARTABLE: [InstancePhase; 5] = [
        InstancePhase::Creating,
        InstancePhase::Bootstrapping,
        InstancePhase::Running,
        InstancePhase::Stopping,
        InstancePhase::Failed,
    ];

    #[test]
    fn a_stopped_instance_of_a_running_cluster_is_started() {
        // The ordinary boot path, and the only one that acts.
        for phase in RESTARTABLE {
            assert_eq!(
                decide(ClusterPhase::Running, phase, Some(&probe(false))),
                Decision::Start,
                "{phase} should be started"
            );
        }
    }

    #[test]
    fn a_running_instance_is_adopted_not_restarted() {
        for phase in RESTARTABLE {
            assert_eq!(
                decide(ClusterPhase::Running, phase, Some(&probe(true))),
                Decision::Adopt,
                "{phase} was already up and should be left alone"
            );
        }
    }

    #[test]
    fn a_fenced_instance_is_never_started() {
        // The split-brain guard. If this ever returns `Start`, a failover
        // races boot recovery helpfully restarting the old primary.
        for phase in BLOCKING {
            for p in [None, Some(&probe(false)), Some(&probe(true))] {
                assert!(
                    matches!(decide(ClusterPhase::Running, phase, p), Decision::Skip(_)),
                    "{phase} must never be acted on, probe running: {:?}",
                    p.map(|p| p.running)
                );
            }
        }
    }

    #[test]
    fn a_cluster_that_is_not_running_has_none_of_its_instances_touched() {
        // The gate that makes a divergence check unnecessary: the only
        // way a container's baked-in spec can disagree with the stored
        // manifest is an interrupted apply, and that cluster is not at
        // `running`.
        for cluster in [
            ClusterPhase::Applying,
            ClusterPhase::Upgrading,
            ClusterPhase::Stopped,
            ClusterPhase::Failed,
            ClusterPhase::Unknown,
        ] {
            for instance in RESTARTABLE {
                assert!(
                    matches!(
                        decide(cluster, instance, Some(&probe(false))),
                        Decision::Skip(_)
                    ),
                    "cluster {cluster} + instance {instance} must be skipped"
                );
            }
        }
    }

    #[test]
    fn a_container_podman_does_not_know_is_reported_never_created() {
        // Creating it would mean an image pull and possibly a bootstrap.
        // Boot recovery does neither, so the only honest answer is to say
        // so and let an operator run `apply`.
        assert_eq!(
            decide(ClusterPhase::Running, InstancePhase::Running, None),
            Decision::Missing
        );
    }

    #[test]
    fn every_skip_carries_a_reason_an_operator_can_act_on() {
        let mut seen = 0;
        for cluster in [ClusterPhase::Applying, ClusterPhase::Running] {
            for instance in BLOCKING.iter().chain(RESTARTABLE.iter()) {
                if let Decision::Skip(why) = decide(cluster, *instance, Some(&probe(false))) {
                    assert!(
                        !why.is_empty(),
                        "{cluster}/{instance} skipped with no reason"
                    );
                    seen += 1;
                }
            }
        }
        assert!(seen > 0, "the test found no skips to check");
    }

    #[test]
    fn the_primary_is_started_before_its_standbys() {
        let rows: Vec<InstanceRecord> = [3u32, 1, 2]
            .into_iter()
            .map(|ordinal| InstanceRecord {
                cluster: "c".into(),
                ordinal,
                container_name: format!("pgpod-c-{ordinal}"),
                container_id: None,
                volume_name: format!("pgpod-c-{ordinal}"),
                host_port: 5432 + ordinal as u16,
                phase: InstancePhase::Running,
                role: pgpod_core::InstanceRole::Unknown,
                timeline: None,
                last_probe_at_ms: None,
            })
            .collect();
        assert_eq!(
            resume_order(rows)
                .iter()
                .map(|r| r.ordinal)
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "ordinal 1 is the primary and standbys should find it up"
        );
    }

    #[test]
    fn a_held_port_is_refused_with_a_message_that_names_it() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = held.local_addr().expect("addr").port();
        let err = port_available(port).expect_err("the port is held by this test");
        assert!(
            err.contains(&port.to_string()),
            "the message must name the port: {err}"
        );
        assert!(
            err.contains("was not started"),
            "and say what did not happen: {err}"
        );
        drop(held);
        assert!(
            port_available(port).is_ok(),
            "a freed port is available again"
        );
    }

    #[test]
    fn two_daemons_cannot_hold_the_lock_at_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = PathLayout::new(
            dir.path().join("config"),
            dir.path().join("state"),
            dir.path().join("data"),
            dir.path().join("run"),
        );

        assert_eq!(DaemonState::probe(&layout), DaemonState::NeverRan);

        let first = DaemonLock::acquire(&layout).expect("the first daemon takes the lock");
        assert_eq!(DaemonState::probe(&layout), DaemonState::Running);

        let err = DaemonLock::acquire(&layout).expect_err("the second must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("already running"),
            "the message should say a daemon is up: {msg}"
        );
        assert!(
            msg.contains(&first.path_held().display().to_string()),
            "and name the lock file: {msg}"
        );

        drop(first);
        assert_eq!(
            DaemonState::probe(&layout),
            DaemonState::NotRunning,
            "the lock file outlives the daemon, but the lock does not"
        );
        DaemonLock::acquire(&layout).expect("the lock is free again");
    }

    #[test]
    fn counts_and_attention_summarise_a_report() {
        let report = ResumeReport {
            clusters: vec![
                ClusterResume {
                    cluster: "a".into(),
                    phase: "running".into(),
                    instances: vec![
                        UnitResume {
                            name: "a-1".into(),
                            action: ResumeAction::Started,
                            detail: None,
                        },
                        UnitResume {
                            name: "a-2".into(),
                            action: ResumeAction::Adopted,
                            detail: None,
                        },
                    ],
                    skipped: None,
                },
                ClusterResume {
                    cluster: "b".into(),
                    phase: "upgrading".into(),
                    instances: Vec::new(),
                    skipped: Some("interrupted".into()),
                },
            ],
            poolers: vec![UnitResume {
                name: "p".into(),
                action: ResumeAction::Missing,
                detail: None,
            }],
        };

        let c = report.counts();
        assert_eq!((c.started, c.adopted, c.missing), (1, 1, 1));
        assert_eq!(
            c.skipped, 1,
            "a skipped cluster counts even with no instances"
        );
        assert!(report.needs_attention(), "a missing pooler needs a human");

        let quiet = ResumeReport {
            clusters: vec![ClusterResume {
                cluster: "a".into(),
                phase: "running".into(),
                instances: vec![UnitResume {
                    name: "a-1".into(),
                    action: ResumeAction::Adopted,
                    detail: None,
                }],
                skipped: None,
            }],
            poolers: Vec::new(),
        };
        assert!(
            !quiet.needs_attention(),
            "an all-adopted boot is uneventful"
        );
    }
}
