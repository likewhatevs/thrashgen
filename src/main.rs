//! Command-line entry point.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

/// Record a container's resource footprint and replay it as synthetic load.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show which measurements this process can take and why
    Caps,
    /// Sample a cgroup, process or command and write a time series
    Record,
    /// Build a profile (averages and percentiles) from recordings
    Aggregate,
    /// Generate load that reproduces a profile and verify it
    Run,
    /// Fit knobs for a profile where counters are available and save them
    Calibrate,
    /// Compare a run report or recording against a profile
    Compare,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Caps
        | Command::Record
        | Command::Aggregate
        | Command::Run
        | Command::Calibrate
        | Command::Compare => bail!("not implemented yet: {:?}", cli.command),
    }
}
