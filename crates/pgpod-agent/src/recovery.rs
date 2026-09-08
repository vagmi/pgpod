//! Restoring a data directory from the repository.
//!
//! This used to be several hundred lines: stream a tar out of object
//! storage, decompress it, feed it to `tar`, hash it on the way past,
//! verify the result against a manifest and against `pg_controldata`. All
//! of that is pgBackRest's now (ADR 04), and what is left is choosing when
//! to invoke it.
//!
//! It still runs inside the instance container, because with the volume
//! storage model that is the only place a PGDATA can be written at all
//! (ADR 00 §4). The volume model makes this cheap: pgBackRest writes
//! straight into `/pgdata/pgdata`, and a failed restore leaves an empty
//! volume and a `Failed` instance rather than a half-written data
//! directory on the host.

use std::path::Path;

use anyhow::{Context, Result};
use pgpod_core::{RecoveryBootstrap, container};

use crate::{info, pgbackrest};

/// Populate PGDATA from the repository.
///
/// `pgbackrest restore` writes its recovery settings into
/// `postgresql.auto.conf` and creates `recovery.signal` itself, so the
/// agent does not have to — and must not, because two writers of the same
/// recovery configuration is exactly the kind of split ownership that goes
/// wrong quietly.
pub fn restore(recovery: &RecoveryBootstrap) -> Result<()> {
    info!(
        "restoring cluster '{}' from its pgbackrest repository",
        recovery.source_stanza
    );

    pgbackrest::restore(
        &recovery.source_stanza,
        recovery.backup_label.as_deref(),
        recovery.target_time.as_deref(),
    )
    .context("pgbackrest restore failed")?;

    // pgBackRest restores the mode PostgreSQL wants, but the directory is
    // created by the agent before the restore runs and PostgreSQL refuses
    // to start on anything looser than 0700.
    std::fs::set_permissions(
        container::PGDATA,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .context("failed to chmod PGDATA to 0700")?;

    info!("restore complete; PostgreSQL will replay WAL from the archive");
    Ok(())
}

/// Whether recovery has finished — PostgreSQL removes the signal file when
/// it promotes.
///
/// Used to decide whether the *next* start is still a recovery or an
/// ordinary primary, so a restored instance that gets restarted after
/// promotion does not try to replay an archive it has already finished
/// with.
pub fn recovery_finished() -> bool {
    !Path::new(container::PGDATA)
        .join("recovery.signal")
        .exists()
}
