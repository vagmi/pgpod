//! Command implementations — argument shuffling and output formatting only.
//!
//! Every one of these is a thin wrapper over a `pgpod_control` call. If
//! logic starts appearing here, it belongs in the library instead
//! (`AGENTS.md` principle 7).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use pgpod_control::{
    ApplyOptions, ApplyReport, BackupReport, ClusterStatus, DeleteReport, Pgpod, PoolerReport,
    RestoreReport,
};
use pgpod_core::{ClusterId, InstanceId, Manifest, PoolerId};
use pgpod_runtime::ExecSpec;
use serde::Serialize;

use crate::output::CommandOutput;

// ---- apply ----------------------------------------------------------

impl CommandOutput for ApplyReport {
    fn to_text(&self) -> String {
        let verb = if self.created { "created" } else { "converged" };
        let mut out = format!(
            "cluster '{}' {} (generation {})\n",
            self.cluster, verb, self.generation
        );
        for i in &self.instances {
            out.push_str(&format!(
                "  {}  container {}  volume {}\n    {}\n",
                i.instance, i.container, i.volume, i.connection_uri
            ));
        }
        out.push_str(&format!("\nConnect with:  pgpod psql {}\n", self.cluster));
        out
    }
}

/// What one `apply -f` produced, whichever kind the file declared.
///
/// `untagged`, so `-o json` prints the report itself rather than wrapping
/// it in a discriminant. A script that already parses an apply report
/// should not have to learn a new envelope because poolers exist.
#[derive(Serialize)]
#[serde(untagged)]
pub enum Applied {
    Cluster(ApplyReport),
    Pooler(PoolerReport),
}

impl CommandOutput for Applied {
    fn to_text(&self) -> String {
        match self {
            Self::Cluster(r) => r.to_text(),
            Self::Pooler(r) => r.to_text(),
        }
    }
}

/// Apply a manifest of either kind.
///
/// Dispatching on `kind` here rather than asking the operator which
/// command to run: a manifest says what it is, and needing a different
/// verb for a file that already declares its own kind is an invitation to
/// get it wrong.
pub async fn apply(path: &str, wait_secs: u64, recreate: bool) -> Result<Applied> {
    let yaml =
        std::fs::read_to_string(path).with_context(|| format!("could not read manifest {path}"))?;
    let manifest = Manifest::from_yaml(&yaml)
        .with_context(|| format!("{path} is not a valid pgpod manifest"))?;

    let pgpod = Pgpod::open()?;
    let wait = Duration::from_secs(wait_secs);
    match manifest {
        Manifest::Cluster(m) => Ok(Applied::Cluster(
            pgpod
                .apply_with(&m, ApplyOptions { wait, recreate })
                .await?,
        )),
        Manifest::Pooler(m) => {
            if recreate {
                // Refused rather than ignored. Recreating a pooler is the
                // one operation it cannot hold for, so a flag that sounds
                // like "do it gracefully" must not be the thing that drops
                // every pooled connection.
                bail!(
                    "--recreate applies to clusters, not poolers. Replacing a \
                     pooler drops every connection through it, because there is \
                     nothing in front of the pooler to hold them: \
                     `pgpod pooler delete {}` then apply again.",
                    m.metadata.name
                );
            }
            Ok(Applied::Pooler(pgpod.apply_pooler(&m, wait).await?))
        }
    }
}

// ---- backup ---------------------------------------------------------

impl CommandOutput for BackupReport {
    fn to_text(&self) -> String {
        let mut out = format!("backup of cluster '{}'\n", self.cluster);
        for d in &self.destinations {
            out.push_str(&format!("  destination:  {d}\n"));
        }
        match (&self.label, &self.backup_type) {
            (Some(label), kind) => {
                out.push_str(&format!("\n  label:        {label}\n"));
                if let Some(k) = kind {
                    // pgBackRest decides full/diff/incr from what the
                    // repository already holds — worth showing, because an
                    // operator asking for "a backup" gets whichever is
                    // cheapest and should know which they got.
                    out.push_str(&format!("  type:         {k}\n"));
                }
                if let Some(size) = self.size_bytes {
                    out.push_str(&format!("  database:     {size} bytes\n"));
                }
                out.push_str("\nComplete — pgbackrest recorded it, so it is restorable.\n");
            }
            (None, _) => out.push_str(
                "\nThe job finished but the repository shows no new backup. \
                 Check `pgpod backups`.\n",
            ),
        }
        out
    }
}

pub async fn backup(cluster: &str, wait_secs: u64) -> Result<BackupReport> {
    let id = ClusterId::new(cluster.to_string())?;
    let pgpod = Pgpod::open()?;
    Ok(pgpod.backup(&id, Duration::from_secs(wait_secs)).await?)
}

/// The backups pgBackRest holds, oldest first.
#[derive(Serialize)]
pub struct BackupList {
    pub cluster: String,
    pub backups: Vec<BackupSummary>,
}

/// Flattened for output. pgBackRest's own JSON is richer than anything
/// `pgpod backups` should print, and `-o json` should be pgpod's shape
/// rather than a passthrough that changes with pgBackRest's version.
#[derive(Serialize)]
pub struct BackupSummary {
    pub label: String,
    pub backup_type: String,
    pub started_at: String,
    pub ended_at: String,
    pub size_bytes: u64,
    pub wal_start: Option<String>,
    pub wal_stop: Option<String>,
}

impl CommandOutput for BackupList {
    fn to_text(&self) -> String {
        if self.backups.is_empty() {
            return format!(
                "cluster '{}' has no backups.\n\nTake one with:  pgpod backup {}\n",
                self.cluster, self.cluster
            );
        }
        let mut out = format!("backups of cluster '{}':\n\n", self.cluster);
        for b in &self.backups {
            out.push_str(&format!(
                "  {:<34}  {:<5}  {:>12} bytes  ended {}\n",
                b.label, b.backup_type, b.size_bytes, b.ended_at
            ));
        }
        if let (Some(first), Some(last)) = (self.backups.first(), self.backups.last()) {
            // The window a PITR target can land in. Saying it here saves
            // an error later.
            out.push_str(&format!(
                "\nRestorable from {} onwards (newest backup ended {}).\n",
                first.ended_at, last.ended_at
            ));
        }
        out
    }
}

pub async fn backups(cluster: &str) -> Result<BackupList> {
    let id = ClusterId::new(cluster.to_string())?;
    let pgpod = Pgpod::open()?;
    let backups = pgpod
        .backups(&id)
        .await?
        .into_iter()
        .map(|b| BackupSummary {
            label: b.label.clone(),
            backup_type: format!("{:?}", b.backup_type).to_lowercase(),
            started_at: b.started_at().format("%Y-%m-%d %H:%M:%SZ").to_string(),
            ended_at: b.ended_at().format("%Y-%m-%d %H:%M:%SZ").to_string(),
            size_bytes: b.size_bytes(),
            wal_start: b.archive.as_ref().and_then(|a| a.start.clone()),
            wal_stop: b.archive.as_ref().and_then(|a| a.stop.clone()),
        })
        .collect();
    Ok(BackupList {
        cluster: cluster.to_string(),
        backups,
    })
}

// ---- restore --------------------------------------------------------

impl CommandOutput for RestoreReport {
    fn to_text(&self) -> String {
        let mut out = format!(
            "restored '{}' into cluster '{}'\n  from backup:  {}\n",
            self.source_cluster, self.target_cluster, self.backup_label
        );
        match &self.target_time {
            Some(t) => out.push_str(&format!("  target time:  {t}\n")),
            None => out.push_str("  target time:  (replay everything available)\n"),
        }
        for i in &self.instances {
            out.push_str(&format!(
                "\n  {}  container {}  volume {}\n    {}\n",
                i.instance, i.container, i.volume, i.connection_uri
            ));
        }
        out.push_str(&format!(
            "\nConnect with:  pgpod psql {}\n",
            self.target_cluster
        ));
        out
    }
}

pub async fn restore(
    source: &str,
    target: &str,
    at: Option<&str>,
    wait_secs: u64,
) -> Result<RestoreReport> {
    let source_id = ClusterId::new(source.to_string())?;
    let target_id = ClusterId::new(target.to_string())?;

    let at = match at {
        None => None,
        Some(raw) => Some(
            chrono::DateTime::parse_from_rfc3339(raw)
                .map(|t| t.with_timezone(&chrono::Utc))
                .with_context(|| {
                    format!(
                        "{raw:?} is not an RFC 3339 timestamp — try \
                         2026-09-04T10:00:00Z"
                    )
                })?,
        ),
    };

    let pgpod = Pgpod::open()?;
    Ok(pgpod
        .restore(&source_id, &target_id, at, Duration::from_secs(wait_secs))
        .await?)
}

// ---- status ---------------------------------------------------------

impl CommandOutput for ClusterStatus {
    fn to_text(&self) -> String {
        let mut out = format!(
            "cluster:    {}\nphase:      {} (generation {})\nimage:      {}\n",
            self.cluster, self.phase, self.generation, self.image
        );
        if let Some(size) = &self.storage_size {
            out.push_str(&format!("storage:    {size} (advisory — not enforced)\n"));
        }
        out.push_str("\ninstances:\n");
        for i in &self.instances {
            out.push_str(&format!(
                "  {:<12} {:<10} {:<9} port {:<6} {}\n",
                i.instance,
                i.role.as_str(),
                if i.running { "running" } else { "stopped" },
                i.host_port,
                i.connection_uri,
            ));
            if i.running && !i.accepting_connections {
                out.push_str(
                    "               (container is up but postgres is not answering yet)\n",
                );
            }
        }
        if !self.poolers.is_empty() {
            out.push_str("\npoolers:\n");
            for p in &self.poolers {
                out.push_str(&format!(
                    "  {:<12} {:<9} port {}\n",
                    p.pooler,
                    if p.running { "running" } else { "stopped" },
                    p.host_port,
                ));
                for uri in &p.connection_uris {
                    out.push_str(&format!("    {uri}\n"));
                }
            }
        }
        if let Some(uri) = self.primary_uri() {
            out.push_str(&format!("\nprimary:    {uri}\n"));
            // Both, deliberately. The pooler is where applications belong
            // — it is what lets a recreate hold connections instead of
            // dropping them — but the direct URI keeps working when the
            // pooler does not, which is the property ADR 02 §6 bought.
            if let Some(p) = self.poolers.iter().find(|p| p.running) {
                if let Some(pooled) = p.connection_uris.first() {
                    out.push_str(&format!("pooled:     {pooled}\n"));
                }
            }
        }
        if !self.recent_events.is_empty() {
            out.push_str("\nrecent events:\n");
            for e in self.recent_events.iter().take(5) {
                out.push_str(&format!("  {e}\n"));
            }
        }
        out
    }
}

#[derive(Serialize)]
pub struct StatusList(pub Vec<ClusterStatus>);

impl CommandOutput for StatusList {
    fn to_text(&self) -> String {
        if self.0.is_empty() {
            return "no clusters — create one with `pgpod apply -f cluster.yaml`\n".to_string();
        }
        let mut out = format!(
            "{:<16} {:<10} {:<10} {}\n",
            "CLUSTER", "PHASE", "INSTANCES", "PRIMARY"
        );
        for c in &self.0 {
            let running = c.instances.iter().filter(|i| i.running).count();
            out.push_str(&format!(
                "{:<16} {:<10} {:<10} {}\n",
                c.cluster,
                c.phase,
                format!("{}/{}", running, c.instances.len()),
                c.primary_uri().unwrap_or("-"),
            ));
        }
        out
    }
}

pub async fn status(cluster: Option<String>) -> Result<StatusOutput> {
    let pgpod = Pgpod::open()?;
    match cluster {
        Some(name) => {
            let id = ClusterId::new(name)?;
            Ok(StatusOutput::One(Box::new(pgpod.status(&id).await?)))
        }
        None => Ok(StatusOutput::Many(StatusList(pgpod.list().await?))),
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum StatusOutput {
    One(Box<ClusterStatus>),
    Many(StatusList),
}

impl CommandOutput for StatusOutput {
    fn to_text(&self) -> String {
        match self {
            Self::One(c) => c.to_text(),
            Self::Many(l) => l.to_text(),
        }
    }
}

// ---- delete ---------------------------------------------------------

impl CommandOutput for DeleteReport {
    fn to_text(&self) -> String {
        let mut out = format!("cluster '{}' deleted\n", self.cluster);
        for c in &self.containers_removed {
            out.push_str(&format!("  removed container {c}\n"));
        }
        for v in &self.volumes_removed {
            out.push_str(&format!("  DESTROYED volume {v}\n"));
        }
        for v in &self.volumes_retained {
            out.push_str(&format!("  kept volume {v} (data intact)\n"));
        }
        if !self.volumes_retained.is_empty() {
            out.push_str(
                "\nData was kept. `pgpod apply` with the same manifest reuses it;\n\
                 `pgpod delete --purge` destroys it.\n",
            );
        }
        if !self.poolers.is_empty() {
            out.push_str(&format!(
                "\nStill fronted by: {}\n\
                 Those poolers now point at an instance that is not running, so \
                 clients\nthrough them will fail until `pgpod apply` brings it \
                 back.\n",
                self.poolers.join(", ")
            ));
        }
        out
    }
}

pub async fn delete(cluster: &str, purge: bool) -> Result<DeleteReport> {
    let pgpod = Pgpod::open()?;
    let id = ClusterId::new(cluster)?;
    Ok(pgpod.delete(&id, purge).await?)
}

// ---- pooler ---------------------------------------------------------

impl CommandOutput for PoolerReport {
    fn to_text(&self) -> String {
        let verb = if self.created { "created" } else { "converged" };
        let mut out = format!(
            "pooler '{}' {} (generation {})\n  container {}\n",
            self.pooler, verb, self.generation, self.container
        );
        for p in &self.pools {
            out.push_str(&format!(
                "  pool {}  ->  cluster {} database {}\n    {}\n",
                p.name, p.cluster, p.database, p.connection_uri
            ));
        }
        out.push_str("\nApplications connect to the pool name, not the database name.\n");
        out
    }
}

pub async fn pooler_list() -> Result<PoolerList> {
    let pgpod = Pgpod::open()?;
    let mut out = Vec::new();
    for record in pgpod.poolers().await? {
        out.push(PoolerSummary {
            pooler: record.name,
            container: record.container_name,
            host_port: record.host_port,
            phase: record.phase,
            pools: record
                .pools
                .into_iter()
                .map(|p| format!("{} -> {}/{}", p.pool_name, p.cluster, p.database))
                .collect(),
        });
    }
    Ok(PoolerList { poolers: out })
}

#[derive(Serialize)]
pub struct PoolerList {
    pub poolers: Vec<PoolerSummary>,
}

#[derive(Serialize)]
pub struct PoolerSummary {
    pub pooler: String,
    pub container: String,
    pub host_port: u16,
    pub phase: String,
    pub pools: Vec<String>,
}

impl CommandOutput for PoolerList {
    fn to_text(&self) -> String {
        if self.poolers.is_empty() {
            return "no poolers\n".to_string();
        }
        let mut out = String::new();
        for p in &self.poolers {
            out.push_str(&format!(
                "{}  {}  port {}\n",
                p.pooler, p.phase, p.host_port
            ));
            for pool in &p.pools {
                out.push_str(&format!("  {pool}\n"));
            }
        }
        out
    }
}

/// `SHOW POOLS`, live from the pooler.
#[derive(Serialize)]
pub struct PoolerPools {
    pools: Vec<pgpod_pooler::PoolStatus>,
}

impl CommandOutput for PoolerPools {
    fn to_text(&self) -> String {
        let pools = &self.pools;
        if pools.is_empty() {
            // Not an error, and worth saying why: pg_doorman creates a
            // pool on the first client connection, so an idle pooler
            // legitimately has none.
            return "no pools are instantiated yet — pg_doorman creates one on \
                    the first client connection\n"
                .to_string();
        }
        let mut out = format!(
            "{:<20} {:<12} {:>7} {:>8} {:>8} {:>8}\n",
            "pool", "user", "paused", "waiting", "active", "idle"
        );
        for p in pools {
            out.push_str(&format!(
                "{:<20} {:<12} {:>7} {:>8} {:>8} {:>8}\n",
                p.database, p.user, p.paused, p.clients_waiting, p.servers_active, p.servers_idle,
            ));
        }
        out
    }
}

pub async fn pooler_pools(pooler: &str) -> Result<PoolerPools> {
    let pgpod = Pgpod::open()?;
    let id = PoolerId::new(pooler)?;
    Ok(PoolerPools {
        pools: pgpod.pooler_pools(&id).await?,
    })
}

#[derive(Serialize)]
pub struct PoolerAction {
    pub pooler: String,
    pub cluster: String,
    pub action: String,
}

impl CommandOutput for PoolerAction {
    fn to_text(&self) -> String {
        format!(
            "{} pools for cluster '{}' on pooler '{}'\n",
            self.action, self.cluster, self.pooler
        )
    }
}

pub async fn pooler_pause(pooler: &str, cluster: &str) -> Result<PoolerAction> {
    let pgpod = Pgpod::open()?;
    let id = PoolerId::new(pooler)?;
    let c = ClusterId::new(cluster)?;
    pgpod.pooler_pause(&id, &c).await?;
    Ok(PoolerAction {
        pooler: pooler.to_string(),
        cluster: cluster.to_string(),
        action: "paused".into(),
    })
}

pub async fn pooler_resume(pooler: &str, cluster: &str) -> Result<PoolerAction> {
    let pgpod = Pgpod::open()?;
    let id = PoolerId::new(pooler)?;
    let c = ClusterId::new(cluster)?;
    pgpod.pooler_resume(&id, &c).await?;
    Ok(PoolerAction {
        pooler: pooler.to_string(),
        cluster: cluster.to_string(),
        action: "resumed".into(),
    })
}

#[derive(Serialize)]
pub struct PoolerDeleted {
    pub pooler: String,
}

impl CommandOutput for PoolerDeleted {
    fn to_text(&self) -> String {
        format!(
            "pooler '{}' deleted\n\nNothing durable was destroyed: a pooler has \
             no volume and re-renders its config on every start.\n",
            self.pooler
        )
    }
}

pub async fn pooler_delete(pooler: &str) -> Result<PoolerDeleted> {
    let pgpod = Pgpod::open()?;
    let id = PoolerId::new(pooler)?;
    pgpod.delete_pooler(&id).await?;
    Ok(PoolerDeleted {
        pooler: pooler.to_string(),
    })
}

// ---- exec / logs / volume -------------------------------------------

#[derive(Serialize)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput for ExecResult {
    fn to_text(&self) -> String {
        let mut out = self.stdout.clone();
        if !self.stderr.is_empty() {
            out.push_str(&self.stderr);
        }
        out
    }
}

pub async fn exec(instance: &str, argv: &[String]) -> Result<ExecResult> {
    if argv.is_empty() {
        bail!("nothing to run — try: pgpod exec {instance} -- psql -U postgres");
    }
    let pgpod = Pgpod::open()?;
    let id: InstanceId = instance.parse()?;
    let container = pgpod.running_container(&id).await?;
    let out = container.exec(&ExecSpec::new(argv.to_vec())).await?;
    Ok(ExecResult {
        exit_code: out.exit_code,
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

#[derive(Serialize)]
pub struct Logs {
    pub instance: String,
    pub logs: String,
}

impl CommandOutput for Logs {
    fn to_text(&self) -> String {
        self.logs.clone()
    }
}

pub async fn logs(instance: &str) -> Result<Logs> {
    let pgpod = Pgpod::open()?;
    let id: InstanceId = instance.parse()?;
    let container = pgpod.podman_container(&id);
    Ok(Logs {
        instance: id.to_string(),
        logs: container.logs_string().await?,
    })
}

#[derive(Serialize)]
pub struct VolumePath {
    pub instance: String,
    pub volume: String,
    pub mountpoint: String,
}

impl CommandOutput for VolumePath {
    fn to_text(&self) -> String {
        format!(
            "volume:     {}\nmountpoint: {}\n\n\
             PGDATA lives inside podman's user namespace, so it is not readable\n\
             directly. To look at it:\n\n  \
             podman unshare ls -la {}/pgdata\n\n\
             Backups are the supported way to get data out — never copy a live\n\
             PGDATA out from under a running postgres.\n",
            self.volume, self.mountpoint, self.mountpoint
        )
    }
}

pub async fn volume_path(instance: &str) -> Result<VolumePath> {
    let pgpod = Pgpod::open()?;
    let id: InstanceId = instance.parse()?;
    let name = id.volume_name();
    let volume = pgpod
        .podman_volume(&name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no volume {name} — has {id} been created?"))?;
    Ok(VolumePath {
        instance: id.to_string(),
        volume: volume.name,
        mountpoint: volume.mountpoint,
    })
}

// ---- psql -----------------------------------------------------------

/// Open an interactive `psql` inside the instance container.
///
/// This is the one place pgpod runs the `podman` binary instead of using
/// the API, and it is a deliberate, narrow exception to the convention in
/// `AGENTS.md`.
///
/// The reason is that an interactive session needs the user's terminal:
/// raw mode, a TTY on both ends, window-resize propagation, and
/// bidirectional streaming. `podman exec -it` already does all of that
/// correctly, and reimplementing it over the REST API would be a
/// meaningful amount of terminal-handling code in service of a
/// human-convenience command. Nothing in the control plane takes this
/// path — only a human at a keyboard.
pub fn psql(cluster: &str, database: Option<&str>) -> Result<std::convert::Infallible> {
    use std::os::unix::process::CommandExt as _;

    let id = ClusterId::new(cluster)?;
    // Phase 1 is single-instance; ordinal 1 is the only instance there is.
    let instance = id.instance(1);
    let database = database.unwrap_or("postgres");

    let socket = pgpod_runtime::default_socket_path();
    let mut cmd = std::process::Command::new("podman");
    cmd.arg("--url")
        .arg(format!("unix://{}", socket.display()))
        .args(["exec", "-it", &instance.container_name()])
        .args([
            "psql",
            "-h",
            pgpod_core::container::SOCKET_DIR,
            "-U",
            "postgres",
            "-d",
            database,
        ]);

    // exec rather than spawn: the terminal belongs to psql from here on,
    // and an intermediate pgpod process would only get in the way of
    // signals and exit codes.
    Err(cmd.exec()).context(
        "could not run `podman` — pgpod uses it for interactive sessions only; \
         is it on PATH?",
    )
}
