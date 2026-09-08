//! pgpod instance manager — PID 1 inside the PostgreSQL container.
//!
//! Statically linked against musl and bind-mounted read-only into an
//! arbitrary PostgreSQL image, so its dependency list is load-bearing: it
//! must never grow `podman-api`, `rusqlite`, or anything pulling in
//! OpenSSL (`adrs/00-project-setup.md` §3).
//!
//! Two roles:
//!
//! 1. **Container entrypoint** (`instance run`) — bootstraps PGDATA,
//!    renders config, then supervises `postgres` and serves `/status`.
//!
//! It no longer ships WAL or takes base backups. PostgreSQL invokes
//! pgBackRest directly as `archive_command` and `restore_command`, and
//! backups run in a job container; the agent renders pgBackRest's
//! configuration and creates its stanza (ADR 04).

mod bootstrap;
mod log;
mod pgbackrest;
mod psql;
mod recovery;
mod secrets;
mod status;
mod supervise;

use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pgpod_core::InstanceSpec;

#[derive(Parser)]
#[command(name = "pgpod-agent", version, about = "pgpod instance manager")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Instance lifecycle. Run as the container entrypoint.
    #[command(subcommand)]
    Instance(InstanceCommand),
}

#[derive(Subcommand)]
enum InstanceCommand {
    /// Bootstrap if needed, then supervise postgres. This is PID 1.
    Run,
    /// Print what postgres currently reports about itself, as JSON.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Instance(InstanceCommand::Run) => run().await,
        Command::Instance(InstanceCommand::Status) => {
            println!("{}", serde_json::to_string_pretty(&status::probe())?);
            Ok(())
        }
    }
}

async fn run() -> Result<()> {
    let spec = InstanceSpec::from_env().context("could not read the instance spec")?;
    info!("starting instance {}", spec.instance);

    let secrets =
        secrets::InstanceSecrets::from_mounts().context("could not read the mounted secrets")?;

    bootstrap::ensure_layout()?;
    let bootstrapped = bootstrap::bootstrap_if_needed(&spec, &secrets)?;

    // Rendered on every start, not just after bootstrap, so a spec change
    // takes effect on restart with no special case.
    bootstrap::render_config(&spec)?;

    if bootstrapped == bootstrap::Bootstrapped::Initdb {
        // Roles must exist before anything can connect over TCP, so this
        // runs against a postmaster bound to the unix socket only.
        //
        // Not on the restored path: a restored cluster arrives with its
        // roles already in it, and rewriting them would change the
        // passwords of a database being restored precisely so that it is
        // as it was.
        supervise::with_local_postgres(|| {
            bootstrap::create_roles(&spec, &secrets)?;
            // While PostgreSQL is up but reachable only over the unix
            // socket, and *before* the real postmaster starts archiving.
            // pgBackRest needs a live server to create a stanza, and every
            // segment archived before the stanza exists is a failure
            // PostgreSQL has to retry — harmless, but it puts errors in
            // the log of a cluster that is working correctly.
            if spec.backup.is_enabled() {
                pgbackrest::ensure_stanza(
                    spec.instance.cluster().as_str(),
                    pgpod_core::container::BOOTSTRAP_PORT,
                )?;
            }
            Ok(())
        })?;
    }

    let child = supervise::spawn_postgres()?;

    // The stanza has to exist before pgBackRest will accept a segment, and
    // creating it needs a running PostgreSQL — pgBackRest connects to read
    // the cluster's identity. So it happens here, after the postmaster is
    // up, rather than during bootstrap.
    //
    // On every start, not only the first: the repository is not pgpod's to
    // assume. It may have been created by an older pgpod, emptied by an
    // operator, or be a destination added to a cluster that already
    // existed.
    // Not on the initdb path, where it already ran above against the
    // bootstrap postmaster.
    if spec.backup.is_enabled() && bootstrapped != bootstrap::Bootstrapped::Initdb {
        let stanza = spec.instance.cluster().as_str().to_string();
        tokio::spawn(async move {
            if supervise::wait_until_ready(Duration::from_secs(120)).await {
                let _ = pgbackrest::ensure_stanza(
                    &stanza,
                    pgpod_core::container::PG_PORT,
                );
            } else {
                warn!(
                    "postgres did not become ready in time, so the pgbackrest \
                     stanza was not created"
                );
            }
        });
    }

    // The status socket outlives individual probes but not the process;
    // it is dropped when the agent exits.
    tokio::spawn(async {
        if let Err(e) = status::serve().await {
            warn!("status socket unavailable: {e}");
        }
    });

    let code = supervise::supervise(child).await?;
    std::process::exit(code);
}
