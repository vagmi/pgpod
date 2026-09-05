//! `pgpod` — the command-line client.
//!
//! Everything here parses arguments, calls the library, and formats the
//! result. No behaviour lives in this crate (`AGENTS.md` principle 7).

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
    long_about = "pgpod manages small PostgreSQL clusters — one primary plus N hot \
                  standbys — on a single host using rootless Podman, with base \
                  backups and WAL shipped to object storage."
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
    ///
    /// Exits non-zero if anything must be fixed, so it can gate a
    /// provisioning script.
    Doctor,
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
    let layout = PathLayout::from_env();

    match cli.command {
        Command::Doctor => {
            let report = doctor::run(&layout).await;
            print!("{}", report.render(cli.output));
            // Diagnostics are the output, so a failing host is a non-zero
            // exit with a clean report — not an anyhow error dumped over
            // the top of it.
            std::process::exit(report.exit_code());
        }
    }
}
