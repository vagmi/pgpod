//! The pooler's control socket.
//!
//! One JSON request per connection, one JSON response, close — the same
//! shape as the instance agent's `/status`, for the same reason: it needs
//! no framing on the reader's side.
//!
//! The daemon reaches this by `podman exec`, running `pgpod-agent pooler
//! pause` and friends inside the container, exactly as `status.rs` already
//! execs `psql` inside an instance. The exec'd process is a client of this
//! socket and does no work itself.
//!
//! **That indirection is the point.** A pause must carry a deadline that
//! outlives the thing that asked for it: if the daemon dies between PAUSE
//! and RESUME, clients would otherwise stay held until their budget
//! expires and then fail, for as long as nobody noticed. PID 1 arms the
//! timer, so the only process that has to survive the window is the one
//! that is already the container.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use pgpod_core::{HumanDuration, container};
use pgpod_pooler::{PoolStatus, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::{Pooler, admin::Admin};
use crate::{info, warn};

pub async fn serve(pooler: Arc<Pooler>) -> Result<()> {
    let path = Path::new(container::POOLER_CONTROL_SOCKET);
    // A socket left by a previous run blocks bind(2).
    let _ = std::fs::remove_file(path);

    let listener =
        UnixListener::bind(path).with_context(|| format!("failed to bind {}", path.display()))?;

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let pooler = Arc::clone(&pooler);
                tokio::spawn(async move {
                    if let Err(e) = handle(pooler, stream).await {
                        warn!("control connection failed: {e}");
                    }
                });
            }
            Err(e) => {
                // A failed accept must not take the agent down — it is
                // supervising the cluster's front door.
                warn!("control accept failed: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle(pooler: Arc<Pooler>, stream: UnixStream) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let response = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => dispatch(&pooler, request)
            .await
            .unwrap_or_else(|e| Response::Error {
                message: e.to_string(),
            }),
        Err(e) => Response::Error {
            message: format!("could not parse the control request: {e}"),
        },
    };

    let mut stream = reader.into_inner();
    stream.write_all(&serde_json::to_vec(&response)?).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    Ok(())
}

async fn dispatch(pooler: &Arc<Pooler>, request: Request) -> Result<Response> {
    match request {
        Request::Pause { cluster, hold } => pause(pooler, &cluster, hold).await,
        Request::Resume { cluster } => {
            let pools = resume(pooler, &cluster).await?;
            Ok(Response::Ok { pools })
        }
        Request::Reconnect { cluster } => {
            let pools = for_each_pool(pooler, &cluster, "RECONNECT").await?;
            Ok(Response::Ok { pools })
        }
        Request::Pools => pools(pooler).await,
        Request::Reload => {
            let mut admin = connect(pooler).await?;
            let result = admin.execute("RELOAD").await;
            admin.close().await;
            result?;
            Ok(Response::Ok { pools: Vec::new() })
        }
    }
}

async fn connect(pooler: &Arc<Pooler>) -> Result<Admin> {
    Admin::connect(
        pooler.spec.port,
        container::POOLER_ADMIN_USER,
        pooler.admin_password.expose(),
        container::POOLER_ADMIN_DB,
    )
    .await
}

/// Pause one cluster's pools, and arm the release.
async fn pause(
    pooler: &Arc<Pooler>,
    cluster: &str,
    hold: Option<HumanDuration>,
) -> Result<Response> {
    let paused = for_each_pool(pooler, cluster, "PAUSE").await?;

    let budget = hold.unwrap_or(pooler.spec.max_hold);
    let generation = {
        let mut holds = pooler.holds.lock().await;
        let entry = holds.entry(cluster.to_string()).or_insert(0);
        *entry += 1;
        *entry
    };

    let armed = Arc::clone(pooler);
    let held = cluster.to_string();
    tokio::spawn(async move {
        let cluster = held;
        tokio::time::sleep(budget.as_std()).await;
        // Per-cluster generation, so a deadline armed for one cluster is
        // not cancelled by a pause or resume on another, and a stale timer
        // from a hold that already ended does nothing.
        let current = armed.holds.lock().await.get(&cluster).copied().unwrap_or(0);
        if current != generation {
            return;
        }
        warn!(
            "hold on {cluster} reached its {} budget with no resume — releasing \
             it. Whatever asked for the hold did not finish, or died.",
            budget.render()
        );
        if let Err(e) = resume(&armed, &cluster).await {
            warn!("could not release the hold on {cluster}: {e}");
        }
    });

    info!(
        "paused {} pool(s) for {cluster}, releasing in {} if nothing else does",
        paused.len(),
        budget.render()
    );
    Ok(Response::Ok { pools: paused })
}

async fn resume(pooler: &Arc<Pooler>, cluster: &str) -> Result<Vec<String>> {
    // Bumped before the command, so a deadline that fires mid-resume sees
    // a stale generation and does not issue a second one.
    {
        let mut holds = pooler.holds.lock().await;
        let entry = holds.entry(cluster.to_string()).or_insert(0);
        *entry += 1;
    }
    for_each_pool(pooler, cluster, "RESUME").await
}

/// Apply one admin verb to every pool belonging to a cluster.
///
/// Per pool rather than the bare verb, which would apply to the whole
/// pooler: on a pooler fronting several clusters, holding one must not
/// stall another's clients (ADR 05 §2).
async fn for_each_pool(pooler: &Arc<Pooler>, cluster: &str, verb: &str) -> Result<Vec<String>> {
    let names: Vec<String> = pooler
        .spec
        .pools_for(cluster)
        .into_iter()
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        anyhow::bail!(
            "this pooler fronts no pools for cluster {cluster:?} — it serves {:?}",
            pooler.spec.clusters()
        );
    }

    let mut admin = connect(pooler).await?;
    let mut applied = Vec::new();
    let mut failure = None;
    for name in names {
        match admin.execute(&format!("{verb} {name}")).await {
            Ok(_) => applied.push(name),
            // pg_doorman creates a pool lazily, on the first client
            // connection, and answers `No pool for database "x"` until
            // then. A pool with no clients has nothing to hold, so this is
            // success — measured, not assumed (ADR 05, consequences).
            Err(e) if is_missing_pool(&e) => {
                info!("no pool for {name} yet — nothing to {verb}");
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    admin.close().await;
    match failure {
        Some(e) => Err(e),
        None => Ok(applied),
    }
}

fn is_missing_pool(e: &anyhow::Error) -> bool {
    e.to_string().contains("No pool for database")
}

async fn pools(pooler: &Arc<Pooler>) -> Result<Response> {
    let mut admin = connect(pooler).await?;
    let result = admin.execute("SHOW POOLS").await;
    admin.close().await;
    let rows = pgpod_pooler::parse_pools(&result?)?;
    Ok(Response::Pools {
        pools: rows
            .into_iter()
            .map(|r| PoolStatus {
                database: r.database,
                user: r.user,
                paused: r.paused,
                clients_waiting: r.clients_waiting,
                servers_active: r.servers_active,
                servers_idle: r.servers_idle,
            })
            .collect(),
    })
}

/// Send one request to a running pooler agent and print the reply.
///
/// This is what the daemon's `podman exec` actually runs.
pub async fn send(request: &Request) -> Result<Response> {
    let path = container::POOLER_CONTROL_SOCKET;
    let stream = UnixStream::connect(path).await.with_context(|| {
        format!(
            "could not reach the pooler agent at {path} — is this container \
             running `pgpod-agent pooler run`?"
        )
    })?;
    let mut reader = BufReader::new(stream);
    let body = serde_json::to_vec(request)?;
    reader.get_mut().write_all(&body).await?;
    reader.get_mut().write_all(b"\n").await?;

    let mut line = String::new();
    reader.read_line(&mut line).await?;
    serde_json::from_str(line.trim()).context("the pooler agent sent an unreadable reply")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lazily_created_pool_is_recognised_as_nothing_to_hold() {
        // pg_doorman creates a pool on the first client connection and
        // answers this until then. Treating it as a failure would abort a
        // switchover over a pool that has no clients to protect.
        let e = anyhow::anyhow!("PAUSE appdb: ERROR: No pool for database \"appdb\"");
        assert!(is_missing_pool(&e));
        assert!(!is_missing_pool(&anyhow::anyhow!(
            "PAUSE appdb: FATAL: password authentication failed"
        )));
    }
}
