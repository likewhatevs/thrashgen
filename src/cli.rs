//! Command-line surface. Each subcommand parses its arguments here and
//! calls into the library; nothing in this module does measurement.

use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::aggregate;
use crate::caps::{Expectations, Facts, Root};
use crate::record;
use crate::samples::Recording;

/// Record a container's resource footprint and replay it as synthetic load.
#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show which measurements this process can take and why
    Caps,
    /// Sample a cgroup, process or command and write a time series
    Record(RecordArgs),
    /// Build a profile (averages and percentiles) from recordings
    Aggregate(AggregateArgs),
    /// Generate load that reproduces a profile and verify it
    Run,
    /// Fit knobs for a profile where counters are available and save them
    Calibrate,
    /// Compare a run report or recording against a profile
    Compare,
}

/// Arguments of `record`.
#[derive(Args, Debug)]
#[command(group = clap::ArgGroup::new("what").required(true).multiple(false))]
pub struct RecordArgs {
    /// A cgroup v2 path relative to the hierarchy root
    #[arg(long, group = "what")]
    cgroup: Option<String>,
    /// An existing process; its threads and future children are followed
    #[arg(long, group = "what")]
    pid: Option<i32>,
    /// The whole machine
    #[arg(long, group = "what")]
    host: bool,
    /// A command to launch and record until it exits
    #[arg(last = true, group = "what")]
    command: Vec<String>,
    /// Spacing of counter and memory samples, seconds
    #[arg(long, default_value_t = 60.0)]
    interval: f64,
    /// Spacing of busy-cores samples, seconds
    #[arg(long, default_value_t = 1.0)]
    cpu_interval: f64,
    /// Stop after this many seconds
    #[arg(long)]
    duration: Option<f64>,
    /// Write the recording here instead of stdout
    #[arg(short, long)]
    output: Option<PathBuf>,
}

/// Arguments of `aggregate`.
#[derive(Args, Debug)]
pub struct AggregateArgs {
    /// Recording files written by `record`
    #[arg(required = true)]
    recordings: Vec<PathBuf>,
    /// Write the profile here instead of stdout
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Window for the burstiness statistic, seconds
    #[arg(long, default_value_t = 300.0)]
    window: f64,
    /// Percentiles to emit, comma separated
    #[arg(long, value_delimiter = ',', default_values_t = aggregate::Options::default().pctiles)]
    pctiles: Vec<f64>,
}

/// Parse arguments and run.
pub fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Aggregate(args) => run_aggregate(args),
        Command::Caps => run_caps(),
        Command::Record(args) => run_record(args),
        Command::Run | Command::Calibrate | Command::Compare => {
            bail!("not implemented yet: {:?}", cli.command)
        }
    }
}

fn run_aggregate(args: AggregateArgs) -> Result<()> {
    let mut recordings = Vec::with_capacity(args.recordings.len());
    for path in &args.recordings {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let rec = Recording::read(BufReader::new(file))
            .with_context(|| format!("read {}", path.display()))?;
        recordings.push(rec);
    }
    let opts = aggregate::Options {
        window_s: args.window,
        pctiles: args.pctiles,
    };
    let profile = aggregate::aggregate(&recordings, &opts)?;
    let text = profile.to_json();
    match args.output {
        Some(path) => {
            std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?
        }
        None => io::stdout().write_all(text.as_bytes())?,
    }
    Ok(())
}

fn run_caps() -> Result<()> {
    let facts = Facts::detect(&Root::default());
    let expectations = Expectations::from_facts(&facts);
    let mut out = io::stdout().lock();
    write!(out, "{facts}\n{expectations}")?;
    Ok(())
}

fn run_record(args: RecordArgs) -> Result<()> {
    let what = if let Some(path) = args.cgroup {
        record::What::Cgroup(path)
    } else if let Some(pid) = args.pid {
        record::What::Pid(pid)
    } else if args.host {
        record::What::Host
    } else {
        record::What::Command(args.command)
    };
    let settings = record::Settings {
        interval_s: args.interval,
        cpu_interval_s: args.cpu_interval,
        duration_s: args.duration,
        child_stdout_to_stderr: args.output.is_none(),
    };
    record::install_stop_handler()?;
    let summary = match args.output {
        Some(path) => {
            let mut file =
                File::create(&path).with_context(|| format!("create {}", path.display()))?;
            record::run(&what, &settings, &mut file)?
        }
        None => record::run(&what, &settings, &mut io::stdout().lock())?,
    };
    for note in &summary.notes {
        eprintln!("note: {note}");
    }
    eprintln!(
        "recorded {} samples and {} busy-cores samples",
        summary.samples, summary.cpu_samples
    );
    Ok(())
}
