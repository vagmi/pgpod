//! Talking to the local PostgreSQL over its unix socket, via `psql`.
//!
//! Shelling out rather than linking a driver is deliberate. A Rust
//! PostgreSQL client would pull in TLS and a connection pool for a job
//! that is a handful of one-shot statements against a unix socket that
//! `peer`-authenticates us. Every dependency here has to survive being
//! statically linked into an arbitrary image (ADR 00 §3), and `psql` is
//! already in every PostgreSQL image by definition.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use pgpod_core::container;

/// Run one statement and return its output, tuples-only and unaligned so
/// the result is trivially parseable.
pub fn query(database: &str, sql: &str) -> Result<String> {
    let out = Command::new("psql")
        // -X ignores ~/.psqlrc, which could otherwise inject settings on
        // an image that ships one.
        .args(["-X", "-v", "ON_ERROR_STOP=1", "-tA"])
        .args(["-h", container::SOCKET_DIR])
        .args(["-U", "postgres", "-d", database])
        .args(["-c", sql])
        .output()
        .context("failed to run psql — is it on PATH in this image?")?;

    if !out.status.success() {
        bail!(
            "psql failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run a statement for effect.
///
/// The statement text is never included in an error, because these carry
/// `CREATE ROLE ... PASSWORD '...'`. Callers pass a `what` describing the
/// operation instead.
pub fn execute(database: &str, sql: &str, what: &str) -> Result<()> {
    let out = Command::new("psql")
        .args(["-X", "-v", "ON_ERROR_STOP=1", "-q"])
        .args(["-h", container::SOCKET_DIR])
        .args(["-U", "postgres", "-d", database])
        .args(["-c", sql])
        .output()
        .context("failed to run psql")?;

    if !out.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Whether a PGDATA directory has been initialised.
///
/// `PG_VERSION` is what `initdb` writes last-ish and what PostgreSQL
/// itself checks. Testing for the directory's existence would be wrong —
/// the agent creates it before `initdb` runs — and testing for emptiness
/// would misfire on a mount point carrying `lost+found`.
pub fn is_initialised(pgdata: &Path) -> bool {
    pgdata.join("PG_VERSION").is_file()
}
