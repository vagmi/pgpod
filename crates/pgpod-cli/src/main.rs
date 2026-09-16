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

    /// Bring back the clusters that were running, then stay up.
    ///
    /// This is what a host reboot needs: pgpod's containers are created
    /// with no podman restart policy, so nothing starts them on their own.
    /// Installed as a `systemd --user` unit by ops/install-pgpod.sh.
    Daemon {
        /// Resume once and exit, instead of staying up.
        ///
        /// The same work the unit does at boot, but reported on stdout —
        /// useful to bring a host back by hand, and to see what the daemon
        /// would do without starting one.
        #[arg(long)]
        once: bool,

        /// Report what resume would do, and change nothing.
        ///
        /// Takes no daemon lock and writes no events, so it is safe to ask
        /// while the unit is running — which is when the question usually
        /// comes up. Implies `--once`.
        #[arg(long)]
        dry_run: bool,
    },

    /// Create or converge a cluster or pooler from a manifest.
    Apply {
        /// Path to the manifest. Its `kind` selects what is applied.
        #[arg(short = 'f', long = "file")]
        file: String,
        /// Seconds to wait for the instance to accept connections.
        /// `0` returns as soon as the container is started.
        #[arg(long, default_value_t = 180)]
        wait: u64,
        /// Replace an instance whose running spec differs from the
        /// manifest.
        ///
        /// The instance spec is fixed when the container is created, so
        /// applying a changed manifest means replacing the container.
        /// With a pooler in front, clients are held at the pooler and
        /// released afterwards; without one, every open connection drops.
        /// Never implicit, for that reason.
        #[arg(long)]
        recreate: bool,
    },

    /// Inspect and control connection poolers.
    #[command(subcommand)]
    Pooler(PoolerCommand),

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
        /// Image for the restored cluster. Needed only to restore a
        /// backup taken *before* a major upgrade, which can only be read
        /// by the PostgreSQL that wrote it.
        #[arg(long)]
        image: Option<String>,
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
        /// Image for the forked cluster. See `restore --image`.
        #[arg(long)]
        image: Option<String>,
        #[arg(long, default_value_t = 1800)]
        wait: u64,
    },

    /// Upgrade a cluster to a new PostgreSQL major version.
    ///
    /// Holds clients at the pooler, runs `pg_upgrade` in a job container,
    /// and brings the instance back on the new image. With no pooler in
    /// front it is an ordinary outage; with one, open connections wait
    /// (ADR 06).
    Upgrade {
        cluster: String,
        /// Image to upgrade to, e.g. `postgres:18`. Its major version is
        /// read from the image, not from the tag.
        #[arg(long = "to-image")]
        to_image: String,
        /// `link` hard-links the data files: seconds regardless of size,
        /// and one-way — the old cluster cannot be started afterwards.
        /// `copy` leaves the old cluster startable and takes as long as
        /// the data is big.
        #[arg(long, default_value = "link", value_parser = parse_method)]
        method: pgpod_core::UpgradeMethod,
        /// `pg_upgrade --jobs`.
        #[arg(long, default_value_t = 2)]
        jobs: u32,
        /// Rehearse it: run `pg_upgrade --check` and put the cluster back
        /// as it was. Still stops the instance — pg_upgrade cannot read a
        /// running cluster — so it is a short outage that changes nothing.
        #[arg(long)]
        check: bool,
        /// Skip `vacuumdb --analyze-in-stages` afterwards. pg_upgrade does
        /// not carry planner statistics over, so skipping this means the
        /// upgraded cluster plans against none.
        #[arg(long)]
        no_analyze: bool,
        /// Skip the post-upgrade backup. The repository's existing backups
        /// belong to the pre-upgrade cluster.
        #[arg(long)]
        no_backup: bool,
        /// Seconds to wait for the upgraded instance to accept connections.
        #[arg(long, default_value_t = 300)]
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

#[derive(Subcommand)]
enum PoolerCommand {
    /// List poolers and the pools they export.
    List,
    /// Show live pool utilisation, straight from pg_doorman.
    Pools { pooler: String },
    /// Hold one cluster's pools.
    ///
    /// The hold is released automatically when the pooler's `maxHold`
    /// budget expires, so a forgotten pause cannot wedge a cluster.
    Pause { pooler: String, cluster: String },
    /// Release one cluster's pools.
    Resume { pooler: String, cluster: String },
    /// Remove a pooler. Destroys nothing durable.
    Delete { pooler: String },
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
        Command::Daemon { once, dry_run } => {
            // The long-running mode returns nothing to print: its report
            // went to the journal, and it only gets here on shutdown.
            if let Some(report) = commands::daemon(once, dry_run).await? {
                emit(report, format);
            }
        }
        Command::Apply {
            file,
            wait,
            recreate,
        } => emit(commands::apply(&file, wait, recreate).await?, format),
        Command::Pooler(PoolerCommand::List) => emit(commands::pooler_list().await?, format),
        Command::Pooler(PoolerCommand::Pools { pooler }) => {
            emit(commands::pooler_pools(&pooler).await?, format)
        }
        Command::Pooler(PoolerCommand::Pause { pooler, cluster }) => {
            emit(commands::pooler_pause(&pooler, &cluster).await?, format)
        }
        Command::Pooler(PoolerCommand::Resume { pooler, cluster }) => {
            emit(commands::pooler_resume(&pooler, &cluster).await?, format)
        }
        Command::Pooler(PoolerCommand::Delete { pooler }) => {
            emit(commands::pooler_delete(&pooler).await?, format)
        }
        Command::Status { cluster } => emit(commands::status(cluster).await?, format),
        Command::Backup { cluster, wait } => {
            emit(commands::backup(&cluster, wait).await?, format)
        }
        Command::Backups { cluster } => emit(commands::backups(&cluster).await?, format),
        Command::Restore {
            cluster,
            target,
            at,
            image,
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
            image,
            wait,
        } => emit(
            commands::restore(&cluster, &target, at.as_deref(), image.as_deref(), wait).await?,
            format,
        ),
        Command::Upgrade {
            cluster,
            to_image,
            method,
            jobs,
            check,
            no_analyze,
            no_backup,
            wait,
        } => emit(
            commands::upgrade(
                &cluster,
                pgpod_control::UpgradeOptions {
                    to_image,
                    method,
                    jobs,
                    check,
                    analyze: !no_analyze,
                    backup: !no_backup,
                    wait: std::time::Duration::from_secs(wait),
                },
            )
            .await?,
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

/// Parse `--method`, so an unknown value is refused by clap with the two
/// that are allowed rather than by the job container half a minute later.
fn parse_method(raw: &str) -> std::result::Result<pgpod_core::UpgradeMethod, String> {
    raw.parse()
}

fn emit(value: impl CommandOutput, format: OutputFormat) {
    print!("{}", value.render(format));
}
