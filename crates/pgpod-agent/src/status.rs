//! The `/status` unix socket.
//!
//! The daemon reads instance role and lag from here rather than opening a
//! SQL connection per probe. That keeps probes off `max_connections` and
//! gives one place to cache, and it is the only route by which the daemon
//! learns an instance's role — which is read from the database, never
//! inferred from the registry (ADR 02 §7).

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};

use pgpod_core::{InstanceRole, container};

use crate::{psql, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub role: InstanceRole,
    /// `false` while PostgreSQL is starting or recovering.
    pub accepting_connections: bool,
    pub last_wal_replay_lsn: Option<String>,
    pub current_wal_flush_lsn: Option<String>,
    pub postmaster_start_time: Option<String>,
    /// Populated when the probe itself failed, so the daemon can tell
    /// "postgres says it is a standby" from "we could not ask".
    pub error: Option<String>,
}

impl Status {
    fn unavailable(error: String) -> Self {
        Self {
            // Not `Standby`, not `Primary`: an instance whose role cannot
            // be read must not be treated as either (ADR 02 §7).
            role: InstanceRole::Unknown,
            accepting_connections: false,
            last_wal_replay_lsn: None,
            current_wal_flush_lsn: None,
            postmaster_start_time: None,
            error: Some(error),
        }
    }
}

/// Ask PostgreSQL what it currently is.
pub fn probe() -> Status {
    // One round trip, unaligned and tuples-only, so parsing is a split.
    const SQL: &str = "SELECT pg_is_in_recovery(), \
                       coalesce(pg_last_wal_replay_lsn()::text, ''), \
                       coalesce(pg_current_wal_flush_lsn()::text, ''), \
                       pg_postmaster_start_time()";

    match psql::query("postgres", SQL) {
        Err(e) => Status::unavailable(e.to_string()),
        Ok(row) => {
            let fields: Vec<&str> = row.split('|').collect();
            if fields.len() < 4 {
                return Status::unavailable(format!("unexpected probe output: {row:?}"));
            }
            // `pg_current_wal_flush_lsn()` errors on a standby, which is
            // why it is wrapped in coalesce and why an empty string here
            // is normal rather than a fault.
            let in_recovery = fields[0] == "t";
            Status {
                role: InstanceRole::from_in_recovery(in_recovery),
                accepting_connections: true,
                last_wal_replay_lsn: non_empty(fields[1]),
                current_wal_flush_lsn: non_empty(fields[2]),
                postmaster_start_time: non_empty(fields[3]),
                error: None,
            }
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Serve `/status` until the process exits.
///
/// One JSON object per connection, then close — the simplest protocol that
/// needs no framing on the reader's side.
pub async fn serve() -> Result<()> {
    let path = Path::new(container::STATUS_SOCKET);
    // A socket left by a previous run blocks bind(2).
    let _ = std::fs::remove_file(path);

    let listener =
        UnixListener::bind(path).with_context(|| format!("failed to bind {}", path.display()))?;

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(async move {
                    if let Err(e) = handle(stream).await {
                        warn!("status connection failed: {e}");
                    }
                });
            }
            Err(e) => {
                // A failed accept must not take the agent down — it is
                // supervising a database.
                warn!("status accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle(mut stream: UnixStream) -> Result<()> {
    // `probe` shells out to psql, so it runs on the blocking pool rather
    // than stalling the reactor.
    let status = tokio::task::spawn_blocking(probe)
        .await
        .context("status probe task panicked")?;
    let body = serde_json::to_vec(&status).context("failed to encode status")?;
    stream.write_all(&body).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreachable_postgres_reports_unknown_not_a_role() {
        // Defaulting to Primary or Standby here is how a split brain
        // starts: the daemon would act on a role nobody confirmed.
        let s = Status::unavailable("connection refused".into());
        assert_eq!(s.role, InstanceRole::Unknown);
        assert!(!s.accepting_connections);
        assert!(s.error.is_some());
    }

    #[test]
    fn status_round_trips_as_json() {
        let s = Status::unavailable("nope".into());
        let encoded = serde_json::to_string(&s).unwrap();
        let decoded: Status = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.role, s.role);
        assert_eq!(decoded.error, s.error);
    }

    #[test]
    fn empty_lsn_fields_become_none() {
        // A standby has no current_wal_flush_lsn; the coalesce turns that
        // into an empty string, which must not surface as Some("").
        assert_eq!(non_empty("  "), None);
        assert_eq!(non_empty(""), None);
        assert_eq!(non_empty("0/3000028"), Some("0/3000028".to_string()));
    }
}
