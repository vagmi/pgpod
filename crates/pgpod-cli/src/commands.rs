//! Command implementations — argument shuffling and output formatting only.
//!
//! Every one of these is a thin wrapper over a `pgpod_control` call. If
//! logic starts appearing here, it belongs in the library instead
//! (`AGENTS.md` principle 7).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use pgpod_control::{ApplyReport, ClusterStatus, DeleteReport, Pgpod};
use pgpod_core::{ClusterId, ClusterManifest, InstanceId};
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

pub async fn apply(path: &str, wait_secs: u64) -> Result<ApplyReport> {
    let yaml =
        std::fs::read_to_string(path).with_context(|| format!("could not read manifest {path}"))?;
    let manifest = ClusterManifest::from_yaml(&yaml)
        .with_context(|| format!("{path} is not a valid pgpod manifest"))?;

    let pgpod = Pgpod::open()?;
    Ok(pgpod
        .apply(&manifest, Duration::from_secs(wait_secs))
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
        if let Some(uri) = self.primary_uri() {
            out.push_str(&format!("\nprimary:    {uri}\n"));
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
        out
    }
}

pub async fn delete(cluster: &str, purge: bool) -> Result<DeleteReport> {
    let pgpod = Pgpod::open()?;
    let id = ClusterId::new(cluster)?;
    Ok(pgpod.delete(&id, purge).await?)
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
