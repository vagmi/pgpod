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
//! 2. **Pooler entrypoint** (`pooler run`) — renders pg_doorman's config,
//!    supervises it, and serves a control socket. Same division of labour,
//!    different container (ADR 05 §5). The remaining `pooler` subcommands
//!    are clients of that socket, run by the daemon through `podman exec`.
//!
//! It no longer ships WAL or takes base backups. PostgreSQL invokes
//! pgBackRest directly as `archive_command` and `restore_command`, and
//! backups run in a job container; the agent renders pgBackRest's
//! configuration and creates its stanza (ADR 04).

mod bootstrap;
mod log;
mod pgbackrest;
mod pooler;
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

    /// Pooler lifecycle and control.
    #[command(subcommand)]
    Pooler(PoolerCommand),
}

#[derive(Subcommand)]
enum PoolerCommand {
    /// Render the config, then supervise pg_doorman. This is PID 1.
    Run,
    /// Hold one cluster's pools.
    ///
    /// Always deadlined: PID 1 releases the hold when the budget expires
    /// whether or not anyone asks it to, so a caller that dies mid-window
    /// cannot leave clients held (ADR 05, consequences).
    Pause {
        cluster: String,
        /// Override the spec's `maxHold` for this hold. Needs a unit.
        #[arg(long, value_name = "DURATION")]
        hold: Option<String>,
    },
    /// Release one cluster's pools.
    Resume { cluster: String },
    /// Recycle backend connections for one cluster's pools.
    Reconnect { cluster: String },
    /// Print `SHOW POOLS` as JSON.
    Pools,
    /// Ask pg_doorman to re-read its config.
    Reload,
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
        Command::Pooler(PoolerCommand::Run) => pooler::run().await,
        Command::Pooler(other) => control(other).await,
    }
}

/// Send one control request to the pooler agent and print its reply.
///
/// Exits non-zero when the pooler reports an error, so `podman exec`
/// carries the failure back to the daemon rather than making it parse
/// stdout to find out whether a hold was actually taken.
async fn control(command: PoolerCommand) -> Result<()> {
    let request = match command {
        PoolerCommand::Run => unreachable!("handled above"),
        PoolerCommand::Pause { cluster, hold } => {
            let hold = match hold {
                Some(raw) => Some(
                    raw.parse::<pgpod_core::HumanDuration>()
                        .map_err(|e| anyhow::anyhow!("--hold: {e}"))?,
                ),
                None => None,
            };
            pooler::Request::Pause { cluster, hold }
        }
        PoolerCommand::Resume { cluster } => pooler::Request::Resume { cluster },
        PoolerCommand::Reconnect { cluster } => pooler::Request::Reconnect { cluster },
        PoolerCommand::Pools => pooler::Request::Pools,
        PoolerCommand::Reload => pooler::Request::Reload,
    };

    let response = pooler::send(&request).await?;
    println!("{}", serde_json::to_string(&response)?);
    if let pooler::Response::Error { message } = response {
        // The message is already on stdout as JSON; this is the exit code
        // the caller actually branches on.
        anyhow::bail!("{message}");
    }
    Ok(())
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
                let _ = pgbackrest::ensure_stanza(&stanza, pgpod_core::container::PG_PORT);
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
