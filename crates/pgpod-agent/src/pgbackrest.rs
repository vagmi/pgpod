//! Running pgBackRest from inside the instance container.
//!
//! The agent's involvement is deliberately small. It renders the
//! configuration and creates the stanza; PostgreSQL invokes `archive-push`
//! and `archive-get` itself, directly, with no pgpod process in between
//! (ADR 04 §1).

use std::process::Command;

use anyhow::{Context, Result, bail};
use pgpod_core::container;

use crate::{info, warn};

/// Run one pgBackRest command, returning its stdout.
///
/// Always with an explicit `--config` and `--stanza`, so nothing depends
/// on pgBackRest's configuration search order — which looks in `/etc`,
/// a directory the container's read-only rootfs makes irrelevant anyway.
pub fn run(stanza: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(container::PGBACKREST_BIN)
        .arg(format!("--config={}", container::PGBACKREST_CONF))
        .arg(format!("--stanza={stanza}"))
        .args(args)
        .output()
        .with_context(|| {
            format!(
                "failed to run {} — the pgBackRest bundle is not mounted. \
                 Build it with ops/build-pgbackrest.sh",
                container::PGBACKREST_BIN
            )
        })?;

    if !out.status.success() {
        // *Both* streams. pgBackRest writes its console log to stdout and
        // only some errors to stderr, so reporting stderr alone produced
        // `failed (exit status: 75):` with nothing after the colon — an
        // error message that says strictly less than the exit code.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail: String = [stderr.trim(), stdout.trim()]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "pgbackrest {} failed ({}):\n{}",
            args.join(" "),
            out.status,
            if detail.is_empty() {
                "(no output)".to_string()
            } else {
                detail
            }
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Create the stanza if the repository does not have it yet.
///
/// `port` is where PostgreSQL is listening *now*, which is not always the
/// configured one: during bootstrap it is on a private port.
///
/// Runs on every start, not only after bootstrap, because the repository
/// is not pgpod's to assume: it may have been created by an older pgpod,
/// emptied by an operator, or be a brand new destination added to a
/// cluster that already existed.
///
/// **Requires a running PostgreSQL** — pgBackRest connects to read the
/// cluster's identity — so this cannot happen before the postmaster is up.
///
/// A failure is a warning, not a fatal error, and that is a deliberate
/// choice in the direction ADR 01 §1 already argues for: if the stanza
/// really is missing, `archive_command` fails on the very next segment,
/// PostgreSQL retains the WAL and `pg_wal` grows visibly. Refusing to
/// start the instance would take a database offline for a backup-system
/// problem, which is a worse trade than letting it run and archive loudly.
pub fn ensure_stanza(stanza: &str, port: u16) -> Result<()> {
    // `stanza-create` is idempotent when the stanza already exists and
    // matches; pgBackRest reports "stanza already exists" and exits 0.
    //
    // `--pg1-port` overrides the rendered config because during bootstrap
    // PostgreSQL is on a private port (`container::BOOTSTRAP_PORT`), and
    // pgBackRest has to reach the server that is actually running.
    let port = format!("--pg1-port={port}");
    match run(stanza, &["stanza-create", &port]) {
        Ok(_) => {
            info!("pgbackrest stanza '{stanza}' ready");
            Ok(())
        }
        Err(e) => {
            warn!(
                "could not create the pgbackrest stanza: {e}\n\
                 Archiving will fail until this is fixed, and pg_wal will grow \
                 — which is the intended back-pressure, not a second bug."
            );
            Ok(())
        }
    }
}

/// Restore a data directory from the repository.
///
/// Everything about *which* backup and *what* target is decided by the
/// daemon and passed in; this only runs the command. The verification that
/// ADR 01 §3 had pgpod doing by hand — that the base backup and the WAL
/// belong to the same cluster — is pgBackRest's own, and it is not
/// optional there either.
pub fn restore(stanza: &str, set: Option<&str>, target_time: Option<&str>) -> Result<()> {
    let mut args: Vec<String> = vec!["restore".into()];

    if let Some(label) = set {
        args.push(format!("--set={label}"));
    }
    if let Some(t) = target_time {
        args.push("--type=time".into());
        args.push(format!("--target={t}"));
        // Without this PostgreSQL pauses on reaching the target and waits
        // for an operator, which for pgpod means an instance that never
        // becomes ready and a `restore` that appears to hang.
        args.push("--target-action=promote".into());
    }

    // pgBackRest writes recovery settings into postgresql.auto.conf. pgpod
    // owns conf.d/ and pg_hba.conf but not that file, so the two do not
    // collide — and letting pgBackRest write its own recovery settings is
    // the point of delegating to it.
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    info!("restoring with pgbackrest: {}", args.join(" "));
    run(stanza, &argv)?;
    Ok(())
}
