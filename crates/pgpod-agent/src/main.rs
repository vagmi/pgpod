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
//! 2. **Invoked by PostgreSQL itself** (`wal archive`, `wal restore`) as
//!    `archive_command` and `restore_command`. Landing in Phase 2.

mod bootstrap;
mod log;
mod psql;
mod secrets;
mod status;
mod supervise;

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
    /// WAL shipping. Invoked by PostgreSQL, not by a human.
    #[command(subcommand)]
    Wal(WalCommand),
}

#[derive(Subcommand)]
enum InstanceCommand {
    /// Bootstrap if needed, then supervise postgres. This is PID 1.
    Run,
    /// Print what postgres currently reports about itself, as JSON.
    Status,
}

#[derive(Subcommand)]
enum WalCommand {
    /// `archive_command` — ship one segment to object storage.
    Archive { path: String },
    /// `restore_command` — fetch one segment from object storage.
    Restore { name: String, target: String },
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
        Command::Wal(_) => {
            // Deliberately an error, not a silent success. An
            // archive_command that exits 0 without storing anything tells
            // PostgreSQL it may recycle WAL that was never archived —
            // the exact silent data loss ADR 01 §1 exists to prevent.
            anyhow::bail!("WAL shipping is not implemented yet (Phase 2)")
        }
    }
}

async fn run() -> Result<()> {
    let spec = InstanceSpec::from_env().context("could not read the instance spec")?;
    info!("starting instance {}", spec.instance);

    let secrets =
        secrets::InstanceSecrets::from_mounts().context("could not read the mounted secrets")?;

    bootstrap::ensure_layout()?;
    let fresh = bootstrap::initdb_if_needed(&spec, &secrets)?;

    // Rendered on every start, not just after bootstrap, so a spec change
    // takes effect on restart with no special case.
    bootstrap::render_config(&spec)?;

    if fresh {
        // Roles must exist before anything can connect over TCP, so this
        // runs against a postmaster bound to the unix socket only.
        supervise::with_local_postgres(|| bootstrap::create_roles(&spec, &secrets))?;
    }

    let child = supervise::spawn_postgres()?;

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
