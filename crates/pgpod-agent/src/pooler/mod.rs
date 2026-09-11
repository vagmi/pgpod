//! pgpod pooler manager — PID 1 inside the pg_doorman container.
//!
//! The same division of labour as the instance agent (ADR 00 §6): the
//! container manages itself, the daemon decides. This side renders
//! pg_doorman's configuration, supervises it, and answers control
//! requests; the daemon says *when*.
//!
//! It has to be PID 1 rather than the daemon delivering a config file,
//! and the reason is a measurement rather than a preference. Rootless
//! podman maps the host user to container UID 0, so a bind-mounted config
//! owned by the host user is unreadable to the container's own uid; a
//! podman secret is chowned correctly but is fixed at container-create
//! time, leaving `RELOAD` nothing new to read. Rendering in-container is
//! the only delivery that is both readable and rewritable (ADR 05 §5).

mod admin;
mod control;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use pgpod_core::{PoolerSpec, Secret, container};

use crate::{info, warn};

pub use control::send;
pub use pgpod_pooler::{Request, Response};

/// Everything PID 1 needs to answer a control request.
pub struct Pooler {
    pub spec: PoolerSpec,
    pub admin_password: Secret,
    /// Per-cluster hold generation, so a pause deadline armed for one
    /// cluster is not cancelled by a pause on another.
    holds: tokio::sync::Mutex<BTreeMap<String, u64>>,
}

/// Read the secrets podman mounted for this pooler.
fn read_secrets(spec: &PoolerSpec) -> Result<(Secret, BTreeMap<String, String>)> {
    let admin = read_secret(container::SECRET_POOLER_ADMIN)?;
    let mut lookups = BTreeMap::new();
    for pool in &spec.pools {
        let path = container::pooler_lookup_secret(pool.lookup_secret_index);
        lookups.insert(pool.name.clone(), read_secret(&path)?.expose().to_string());
    }
    Ok((admin, lookups))
}

fn read_secret(path: &str) -> Result<Secret> {
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "required secret {path} is not mounted — the daemon did not attach \
             it to this container"
        )
    })?;
    // Same trimming as the instance agent: a trailing newline is easy to
    // introduce and authenticates against nothing.
    let value = raw.trim_end_matches(['\n', '\r']).to_string();
    if value.is_empty() {
        bail!("secret {path} is empty");
    }
    Ok(Secret::new(value))
}

/// Render the config into the tmpfs, at a mode that is not world-readable.
///
/// The directory is a fresh tmpfs, which mounts `1777` — that is what
/// makes it writable by a non-root uid at all, and also why the file's
/// own mode has to be set rather than inherited from it. The file holds
/// the `auth_query` and admin passwords in plaintext because pg_doorman
/// accepts no other form.
fn render_config(
    spec: &PoolerSpec,
    admin: &Secret,
    lookups: BTreeMap<String, String>,
) -> Result<()> {
    let rendered = pgpod_pooler::render(
        spec,
        &pgpod_pooler::Secrets {
            admin_password: admin.expose().to_string(),
            lookup_passwords: lookups,
        },
    )
    .context("could not render the pg_doorman configuration")?;

    std::fs::create_dir_all(container::POOLER_DIR)
        .with_context(|| format!("could not create {}", container::POOLER_DIR))?;
    std::fs::write(container::POOLER_CONF, rendered)
        .with_context(|| format!("could not write {}", container::POOLER_CONF))?;
    std::fs::set_permissions(
        container::POOLER_CONF,
        std::fs::Permissions::from_mode(container::POOLER_CONF_MODE),
    )
    .context("could not restrict the rendered config's permissions")?;
    Ok(())
}

fn spawn_pg_doorman() -> Result<Child> {
    info!("starting pg_doorman");
    Command::new(container::POOLER_BIN)
        .arg(container::POOLER_CONF)
        .spawn()
        .with_context(|| {
            format!(
                "failed to start {} — is pg_doorman on PATH in this image?",
                container::POOLER_BIN
            )
        })
}

/// PID 1: render, supervise, serve control.
pub async fn run() -> Result<()> {
    let spec = PoolerSpec::from_env().context("could not read the pooler spec")?;
    info!("starting pooler {}", spec.pooler);

    let (admin_password, lookups) = read_secrets(&spec)?;
    render_config(&spec, &admin_password, lookups)?;

    let child = spawn_pg_doorman()?;

    let pooler = Arc::new(Pooler {
        spec,
        admin_password,
        holds: tokio::sync::Mutex::new(BTreeMap::new()),
    });

    let serving = Arc::clone(&pooler);
    tokio::spawn(async move {
        if let Err(e) = control::serve(serving).await {
            warn!("control socket unavailable: {e}");
        }
    });

    let code = supervise(child).await?;
    std::process::exit(code);
}

/// Supervise pg_doorman until it exits, translating signals.
///
/// **SIGTERM, never SIGINT.** The upstream image sets `STOPSIGNAL SIGTERM`
/// deliberately: SIGINT in a non-TTY triggers pg_doorman's binary-upgrade
/// path, which spawns a child, hands over the listening socket and exits —
/// and PID 1 exiting takes the container with it. Verified by sending one
/// (ADR 05 §6). So a SIGINT arriving here is forwarded as SIGTERM too.
async fn supervise(child: Child) -> Result<i32> {
    use tokio::signal::unix::{SignalKind, signal};

    let pid = Pid::from_raw(child.id() as i32);
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
    let mut sighup = signal(SignalKind::hangup()).context("failed to install SIGHUP handler")?;
    let mut sigchld = signal(SignalKind::child()).context("failed to install SIGCHLD handler")?;

    // As in `supervise.rs`: `Child::wait` would compete with the explicit
    // `waitpid` below, and only one of them can reap a given pid.
    std::mem::forget(child);

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("received SIGTERM, stopping pg_doorman");
                let _ = kill(pid, Signal::SIGTERM);
            }
            _ = sigint.recv() => {
                // Translated, not forwarded. See the doc comment.
                info!("received SIGINT, stopping pg_doorman with SIGTERM");
                let _ = kill(pid, Signal::SIGTERM);
            }
            _ = sighup.recv() => {
                info!("received SIGHUP, asking pg_doorman to reload");
                let _ = kill(pid, Signal::SIGHUP);
            }
            _ = sigchld.recv() => {
                if let Some(code) = reap(pid)? {
                    return Ok(code);
                }
            }
        }
    }
}

fn reap(doorman: Pid) -> Result<Option<i32>> {
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => {
                if pid == doorman {
                    info!("pg_doorman exited with code {code}");
                    return Ok(Some(code));
                }
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                if pid == doorman {
                    warn!("pg_doorman was killed by {sig:?}");
                    return Ok(Some(128 + sig as i32));
                }
            }
            Ok(WaitStatus::StillAlive) => return Ok(None),
            Ok(_) => {}
            Err(nix::errno::Errno::ECHILD) => {
                bail!("pg_doorman disappeared without reporting an exit status")
            }
            Err(e) => bail!("waitpid failed: {e}"),
        }
    }
}
