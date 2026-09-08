//! `pgpod` — the command-line client.
//!
//! Parses arguments, calls the library, formats output. No behaviour lives
//! in this crate (`AGENTS.md` principle 7).

mod commands;
mod doctor;
mod output;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pgpod_core::PathLayout;

use crate::output::{CommandOutput, OutputFormat};

#[derive(Parser)]
#[command(
    name = "pgpod",
    version,
    about = "A PostgreSQL operator for rootless Podman",
    long_about = "pgpod manages small PostgreSQL clusters on a single host using \
                  rootless Podman, with base backups and WAL shipped to object \
                  storage."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Output format
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Text, global = true)]
    output: OutputFormat,
}

#[derive(Subcommand)]
enum Command {
    /// Check that this host can run pgpod. Run this first.
    Doctor,

    /// Create or converge a cluster from a manifest.
    Apply {
        /// Path to the cluster manifest.
        #[arg(short = 'f', long = "file")]
        file: String,
        /// Seconds to wait for the instance to accept connections.
        /// `0` returns as soon as the container is started.
        #[arg(long, default_value_t = 180)]
        wait: u64,
    },

    /// Show cluster state. Omit the name to list every cluster.
    Status { cluster: Option<String> },

    /// Open an interactive psql session against a cluster.
    Psql {
        cluster: String,
        /// Database to connect to.
        #[arg(short, long)]
        database: Option<String>,
    },

    /// Run a command inside an instance container, e.g. `pgpod exec mydb-1 -- ls /pgdata`.
    Exec {
        /// Instance, as `<cluster>-<ordinal>`.
        instance: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },

    /// Print an instance's container logs.
    Logs {
        /// Instance, as `<cluster>-<ordinal>`.
        instance: String,
    },

    /// Where an instance's data actually lives, and how to reach it.
    #[command(subcommand)]
    Volume(VolumeCommand),

    /// Take a base backup and ship it to the cluster's destinations.
    Backup {
        cluster: String,
        /// Seconds to wait for the backup to finish. `0` starts the job
        /// and returns.
        #[arg(long, default_value_t = 3600)]
        wait: u64,
    },

    /// List the backups that are actually restorable.
    Backups { cluster: String },

    /// Restore a cluster from its archive into a new one.
    Restore {
        /// Cluster to restore *from*.
        cluster: String,
        /// Name for the restored cluster. Must not already exist.
        #[arg(long = "as")]
        target: String,
        /// Point in time to recover to, RFC 3339, e.g.
        /// `2026-09-04T10:00:00Z`. Omit to replay everything available.
        #[arg(long)]
        at: Option<String>,
        #[arg(long, default_value_t = 1800)]
        wait: u64,
    },

    /// Fork a cluster: restore it as of now, under a new name.
    Fork {
        cluster: String,
        #[arg(long = "as")]
        target: String,
        /// Point in time, RFC 3339. Defaults to now — i.e. everything the
        /// archive has.
        #[arg(long)]
        at: Option<String>,
        #[arg(long, default_value_t = 1800)]
        wait: u64,
    },

    /// Remove a cluster's containers. Volumes are kept unless --purge.
    Delete {
        cluster: String,
        /// Also destroy the volumes. **This deletes the databases.**
        #[arg(long)]
        purge: bool,
    },
}

#[derive(Subcommand)]
enum VolumeCommand {
    /// Print the host path of an instance's volume.
    Path {
        /// Instance, as `<cluster>-<ordinal>`.
        instance: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PGPOD_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let format = cli.output;

    match cli.command {
        Command::Doctor => {
            let layout = PathLayout::from_env();
            let report = doctor::run(&layout).await;
            print!("{}", report.render(format));
            // Diagnostics are the output: a failing host exits non-zero
            // with a clean report, not an error dumped over the top of it.
            std::process::exit(report.exit_code());
        }
        Command::Apply { file, wait } => emit(commands::apply(&file, wait).await?, format),
        Command::Status { cluster } => emit(commands::status(cluster).await?, format),
        Command::Backup { cluster, wait } => {
            emit(commands::backup(&cluster, wait).await?, format)
        }
        Command::Backups { cluster } => emit(commands::backups(&cluster).await?, format),
        Command::Restore {
            cluster,
            target,
            at,
            wait,
        }
        // `fork` is `restore` with the target defaulting to now — paagan's
        // fork command, generalized from a local directory to object
        // storage (ADR 01 §5). One implementation, two names, because the
        // second name is what people reach for.
        | Command::Fork {
            cluster,
            target,
            at,
            wait,
        } => emit(
            commands::restore(&cluster, &target, at.as_deref(), wait).await?,
            format,
        ),
        Command::Delete { cluster, purge } => {
            emit(commands::delete(&cluster, purge).await?, format)
        }
        Command::Exec { instance, argv } => {
            let result = commands::exec(&instance, &argv).await?;
            let code = result.exit_code;
            print!("{}", result.render(format));
            // Propagate the command's own exit code, so `pgpod exec` is
            // usable in a script's `set -e`.
            std::process::exit(code.unwrap_or(1));
        }
        Command::Logs { instance } => emit(commands::logs(&instance).await?, format),
        Command::Volume(VolumeCommand::Path { instance }) => {
            emit(commands::volume_path(&instance).await?, format)
        }
        Command::Psql { cluster, database } => {
            // Never returns on success — it replaces this process.
            commands::psql(&cluster, database.as_deref())?;
            unreachable!("exec replaces the process")
        }
    }

    Ok(())
}

fn emit(value: impl CommandOutput, format: OutputFormat) {
    print!("{}", value.render(format));
}
