//! Running PostgreSQL as a child of PID 1.
//!
//! The agent stays alive as PID 1 rather than `exec`ing PostgreSQL, so it
//! can serve `/status` and translate signals. That makes it responsible
//! for the things PID 1 owes a container:
//!
//! * **Reaping.** Orphans reparent to PID 1. If PID 1 does not `waitpid`
//!   them they accumulate as zombies until the pid table fills. PostgreSQL
//!   reaps its own children, but a backend that outlives a crashed
//!   postmaster lands on us.
//! * **Signal translation.** `podman stop` sends SIGTERM. To PostgreSQL
//!   SIGTERM means *smart* shutdown — wait for every client to disconnect,
//!   which can hang indefinitely and end in SIGKILL and crash recovery.
//!   What we want is SIGINT, PostgreSQL's *fast* shutdown.

use std::process::{Child, Command};

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use pgpod_core::container;

use crate::{info, warn};

/// Spawn the postmaster.
pub fn spawn_postgres() -> Result<Child> {
    info!("starting postgres");
    Command::new("postgres")
        .args(["-D", container::PGDATA])
        .spawn()
        .context("failed to start postgres — is it on PATH in this image?")
}

/// Supervise the postmaster until it exits, translating signals.
///
/// Returns the exit code to propagate.
pub async fn supervise(child: Child) -> Result<i32> {
    use tokio::signal::unix::{SignalKind, signal};

    let pid = Pid::from_raw(child.id() as i32);
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
    let mut sigchld = signal(SignalKind::child()).context("failed to install SIGCHLD handler")?;

    // Deliberately dropped: `Child::wait` would compete with the explicit
    // `waitpid` below, and only one of them can reap a given pid.
    std::mem::forget(child);

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                // SIGTERM to *us* becomes SIGINT to PostgreSQL: fast
                // shutdown, not smart shutdown that waits for clients.
                info!("received SIGTERM, requesting fast shutdown");
                let _ = kill(pid, Signal::SIGINT);
            }
            _ = sigint.recv() => {
                info!("received SIGINT, requesting fast shutdown");
                let _ = kill(pid, Signal::SIGINT);
            }
            _ = sigchld.recv() => {
                if let Some(code) = reap(pid)? {
                    return Ok(code);
                }
            }
        }
    }
}

/// Reap every exited child. Returns the postmaster's exit code once it is
/// the one that exited, `None` if it is still running.
///
/// The loop matters: SIGCHLD is not queued, so several children exiting
/// close together can deliver a single signal.
fn reap(postgres: Pid) -> Result<Option<i32>> {
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => {
                if pid == postgres {
                    info!("postgres exited with code {code}");
                    return Ok(Some(code));
                }
                // An orphan we inherited. Reaped, nothing more to do.
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                if pid == postgres {
                    warn!("postgres was killed by {sig:?}");
                    // Conventional shell encoding, so the container's exit
                    // status distinguishes a signal death from exit(N).
                    return Ok(Some(128 + sig as i32));
                }
            }
            // Nothing left to reap right now.
            Ok(WaitStatus::StillAlive) => return Ok(None),
            Ok(_) => {}
            Err(nix::errno::Errno::ECHILD) => {
                // No children at all. If the postmaster is gone and we
                // never saw its status, treat it as a failure rather than
                // hanging forever waiting for a signal that cannot come.
                bail!("postgres disappeared without reporting an exit status");
            }
            Err(e) => bail!("waitpid failed: {e}"),
        }
    }
}

/// Wait until PostgreSQL is accepting connections on its unix socket.
///
/// `pg_isready` rather than the agent's own status socket: it is the
/// narrowest check available and does not depend on pgpod's code being
/// correct. Returns whether it got there before `timeout`.
pub async fn wait_until_ready(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let ok = Command::new("pg_isready")
            .args(["-h", container::SOCKET_DIR, "-U", "postgres"])
            .args(["-p", &container::PG_PORT.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    false
}

/// Run `f` with PostgreSQL up privately.
///
/// Bootstrap SQL has to run against a live server, but the instance must
/// not be reachable while it still has `initdb`'s state — a client
/// connecting mid-bootstrap could see a half-created role set.
/// `listen_addresses=''` closes TCP; `port` closes the unix socket too,
/// because a unix socket's filename encodes the port. Both are needed:
/// with only the first, `pgpod apply --wait` would see `pg_isready`
/// succeed against *this* postmaster and report an instance ready that
/// was about to shut down.
pub fn with_local_postgres<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    info!("starting postgres locally for bootstrap");
    let start = Command::new("pg_ctl")
        .args(["-D", container::PGDATA, "-w", "start", "-o"])
        .arg(format!(
            "-c listen_addresses='' -c unix_socket_directories={} -c port={}",
            container::SOCKET_DIR,
            container::BOOTSTRAP_PORT
        ))
        .status()
        .context("failed to run pg_ctl start")?;
    if !start.success() {
        bail!("pg_ctl start failed with {start}");
    }

    let result = f();

    // Stop regardless of the outcome: leaving a bootstrap postmaster
    // running would collide with the real one started moments later.
    let stop = Command::new("pg_ctl")
        .args(["-D", container::PGDATA, "-m", "fast", "-w", "stop"])
        .status()
        .context("failed to run pg_ctl stop")?;
    if !stop.success() {
        warn!("pg_ctl stop failed with {stop}");
    }
    info!("bootstrap postgres stopped");

    result
}
