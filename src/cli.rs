//! Command-line surface. Each subcommand parses its arguments here and
//! calls into the library; nothing in this module does measurement.

use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::aggregate;
use crate::calibration::Calibration;
use crate::caps::{Expectations, Facts, Root};
use crate::compare;
use crate::loadgen::Knobs;
use crate::profile::{FIELDS, Pick, Profile, Role, Targets};
use crate::record;
use crate::run;
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
    Run(RunArgs),
    /// Fit knobs for a profile where counters are available and save them
    Calibrate(CalibrateArgs),
    /// Compare a run report, recording or profile against a profile
    Compare(CompareArgs),
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

/// Arguments of `run`.
#[derive(Args, Debug)]
pub struct RunArgs {
    /// Profile written by `aggregate`
    profile: Option<PathBuf>,
    /// Column of the profile to replay: avg or a percentile such as p80
    #[arg(long, default_value = "avg")]
    pick: String,
    /// Override or add a field, as key=value; repeatable. The field
    /// options below do the same and win when both name a field
    #[arg(long = "set", value_name = "KEY=VALUE")]
    sets: Vec<String>,
    #[command(flatten)]
    fields: FieldArgs,
    /// Stop after this many seconds; default runs until interrupted
    #[arg(long)]
    duration: Option<f64>,
    /// Measurement window in seconds; default sizes it from the burstiness target
    #[arg(long)]
    window: Option<f64>,
    /// Disk-backed directory for the page-cache and slab files
    #[arg(long, default_value = ".")]
    scratch_dir: PathBuf,
    /// Pin workers to CPUs: auto, yes or no
    #[arg(long, default_value = "auto")]
    pin: String,
    /// Seed for the streams and the burstiness draw
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Keep adjusting knobs after convergence
    #[arg(long)]
    adaptive: bool,
    /// Fitter passes before holding regardless
    #[arg(long, default_value_t = 15)]
    max_passes: u32,
    /// Worker threads; default derives them from the targets
    #[arg(long)]
    threads: Option<usize>,
    /// Main buffer size, with an optional K, M, G or T suffix
    #[arg(long)]
    main_bytes: Option<String>,
    /// Pages reserved for the sparse stream
    #[arg(long, default_value_t = 16384)]
    sparse_pages: usize,
    /// Executable pages reserved for the code stream
    #[arg(long, default_value_t = 16384)]
    code_pages: usize,
    /// Do not ask for transparent hugepages on the main buffer
    #[arg(long)]
    no_huge: bool,
    /// Fixed knobs as JSON: run them as given, with no fitting
    #[arg(long)]
    knobs: Option<PathBuf>,
    /// Open no PMU counters, leaving them to a recorder; needs --knobs
    #[arg(long, requires = "knobs")]
    no_counters: bool,
    /// Start fitting from a calibration written by `calibrate`
    #[arg(long)]
    calibration: Option<PathBuf>,
    /// Write the report as JSON here
    #[arg(long)]
    report: Option<PathBuf>,
}

/// Arguments of `calibrate`: the run arguments plus where to save.
#[derive(Args, Debug)]
pub struct CalibrateArgs {
    #[command(flatten)]
    run: RunArgs,
    /// Write the calibration here
    #[arg(short, long)]
    output: PathBuf,
}

/// One option per row of the field table, `--ipc 1.2` and so on. The
/// options are generated from [`FIELDS`] so the two cannot drift.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FieldArgs {
    /// Fields given on the command line, in table order.
    pub values: Vec<(&'static str, f64)>,
}

/// The option name of a field: its key with dashes for underscores.
pub fn flag_name(key: &str) -> String {
    key.replace('_', "-")
}

impl clap::FromArgMatches for FieldArgs {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let values = FIELDS
            .iter()
            .filter_map(|f| matches.get_one::<f64>(f.key).map(|v| (f.key, *v)))
            .collect();
        Ok(FieldArgs { values })
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl clap::Args for FieldArgs {
    fn augment_args(mut cmd: clap::Command) -> clap::Command {
        for f in FIELDS {
            let role = match f.role {
                Role::Hint => "hint",
                Role::Target => "target",
                Role::Consistency => "recorded, not fitted",
                Role::Provenance => "provenance",
            };
            let unit = if f.unit.is_empty() {
                String::new()
            } else {
                format!(", {}", f.unit)
            };
            cmd = cmd.arg(
                clap::Arg::new(f.key)
                    .long(flag_name(f.key))
                    .value_name("VALUE")
                    .help(format!("{} ({role}{unit})", f.help))
                    .help_heading("Fields")
                    .value_parser(clap::value_parser!(f64)),
            );
        }
        cmd
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        Self::augment_args(cmd)
    }
}

/// Parse a byte count with an optional K, M, G or T suffix (powers of 1024).
pub fn parse_bytes(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            let m = match c.to_ascii_uppercase() {
                'K' => 1u64 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                'T' => 1 << 40,
                _ => bail!("unknown size suffix in `{s}`"),
            };
            (&s[..s.len() - 1], m)
        }
        _ => (s, 1),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("not a size: `{s}`"))?;
    if n.is_nan() || n < 0.0 {
        bail!("not a size: `{s}`");
    }
    Ok((n * mult as f64) as u64)
}

/// Arguments of `compare`.
#[derive(Args, Debug)]
pub struct CompareArgs {
    /// The profile that was asked for
    expected: PathBuf,
    /// A run report, a recording, or another profile
    actual: PathBuf,
    /// Column of the expected profile: avg or a percentile such as p80
    #[arg(long, default_value = "avg")]
    pick: String,
    /// Column of the actual file when it is a profile
    #[arg(long, default_value = "avg")]
    actual_pick: String,
    /// Relative tolerance
    #[arg(long, default_value_t = 0.05)]
    tolerance: f64,
    /// Write the comparison as JSON here
    #[arg(long)]
    json: Option<PathBuf>,
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
        Command::Run(args) => run_run(args),
        Command::Compare(args) => run_compare(args),
        Command::Calibrate(args) => run_calibrate(args),
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
    let probe = crate::perf::probe();
    let mut out = io::stdout().lock();
    write!(out, "{facts}\n{expectations}")?;
    writeln!(out)?;
    for (what, result) in [
        ("probe: own threads, user mode", &probe.self_user),
        ("probe: own threads, kernel mode", &probe.self_kernel),
        ("probe: per-CPU", &probe.per_cpu),
    ] {
        match result {
            Ok(()) => writeln!(out, "{what:30} ok")?,
            Err(e) => writeln!(out, "{what:30} no   {e}")?,
        }
    }
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

/// Targets from a profile column, then `--set` overrides, then the
/// named field options.
pub fn targets_from(
    profile: Option<&std::path::Path>,
    pick: &str,
    sets: &[String],
    fields: &[(&str, f64)],
) -> Result<Targets> {
    let mut targets = match profile {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let profile =
                Profile::from_json(&text).with_context(|| format!("parse {}", path.display()))?;
            let pick: Pick = pick.parse()?;
            profile.pick(&pick)?
        }
        None => Targets::new(),
    };
    for s in sets {
        targets.set_from_arg(s)?;
    }
    for (key, value) in fields {
        targets.set(key, *value)?;
    }
    if targets.is_empty() {
        bail!("no targets: give a profile, --set key=value or a field option such as --ipc");
    }
    Ok(targets)
}

/// Settings and targets from the run arguments.
fn run_setup(args: &RunArgs) -> Result<(Targets, run::Settings, Vec<String>)> {
    let targets = targets_from(
        args.profile.as_deref(),
        &args.pick,
        &args.sets,
        &args.fields.values,
    )?;
    let pin = match args.pin.as_str() {
        "auto" => run::Pin::Auto,
        "yes" => run::Pin::Yes,
        "no" => run::Pin::No,
        other => bail!("--pin must be auto, yes or no, not `{other}`"),
    };
    let knobs: Option<Knobs> = match &args.knobs {
        Some(path) => Some(serde_json::from_str(
            &std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
        )?),
        None => None,
    };
    let mut notes = Vec::new();
    let mut threads = args.threads;
    let initial_knobs = match &args.calibration {
        Some(path) => {
            let cal = Calibration::load(path)?;
            if cal.matches(crate::cpu::detect().as_ref()) {
                threads = threads.or(Some(cal.threads));
                Some(cal.knobs)
            } else {
                notes.push(format!("calibration {} was fit on a different CPU model; starting from defaults instead", path.display()));
                None
            }
        }
        None => None,
    };
    let settings = run::Settings {
        duration_s: args.duration,
        window_s: args.window,
        scratch_dir: args.scratch_dir.clone(),
        pin,
        seed: args.seed,
        adaptive: args.adaptive,
        max_passes: args.max_passes,
        threads,
        main_bytes: args.main_bytes.as_deref().map(parse_bytes).transpose()?,
        sparse_capacity: args.sparse_pages,
        code_capacity: args.code_pages,
        huge: !args.no_huge,
        counters: !args.no_counters,
        knobs,
        initial_knobs,
        stop_when_converged: false,
    };
    Ok((targets, settings, notes))
}

fn run_run(args: RunArgs) -> Result<()> {
    let (targets, settings, notes) = run_setup(&args)?;
    for n in &notes {
        eprintln!("note: {n}");
    }
    record::install_stop_handler()?;
    let report = run::run(&targets, &settings, &mut io::stderr().lock())?;
    if let Some(path) = &args.report {
        std::fs::write(path, report.to_json())
            .with_context(|| format!("write {}", path.display()))?;
    }
    print!("{report}");
    Ok(())
}

fn run_calibrate(args: CalibrateArgs) -> Result<()> {
    let (targets, mut settings, notes) = run_setup(&args.run)?;
    for n in &notes {
        eprintln!("note: {n}");
    }
    if settings.knobs.is_some() {
        bail!("calibrate fits knobs; --knobs would fix them");
    }
    settings.stop_when_converged = true;
    record::install_stop_handler()?;
    let report = run::run(&targets, &settings, &mut io::stderr().lock())?;
    if let Some(path) = &args.run.report {
        std::fs::write(path, report.to_json())
            .with_context(|| format!("write {}", path.display()))?;
    }
    let cal = Calibration::from_report(&targets, &report);
    cal.save(&args.output)?;
    print!("{report}");
    eprintln!(
        "calibration written to {}{}",
        args.output.display(),
        if cal.converged {
            ""
        } else {
            " (not converged)"
        }
    );
    Ok(())
}

fn run_compare(args: CompareArgs) -> Result<()> {
    let expected = targets_from(Some(&args.expected), &args.pick, &[], &[])?;
    let actual = compare::load_actual(&args.actual)?;
    let pick: Pick = args.actual_pick.parse()?;
    let values = compare::actual_values(&actual, &pick)?;
    let comparison = compare::compare(&expected, &values, args.tolerance);
    if let Some(path) = &args.json {
        std::fs::write(path, comparison.to_json())
            .with_context(|| format!("write {}", path.display()))?;
    }
    print!("{comparison}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes() {
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_bytes("2k").unwrap(), 2048);
        assert_eq!(parse_bytes("1.5M").unwrap(), 3 << 19);
        assert_eq!(parse_bytes(" 4G ").unwrap(), 4 << 30);
        assert_eq!(parse_bytes("1T").unwrap(), 1 << 40);
        assert!(parse_bytes("1X").is_err());
        assert!(parse_bytes("lots").is_err());
        assert!(parse_bytes("-1").is_err());
    }

    #[test]
    fn targets_from_profile_and_sets() {
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/profile.json");
        let t = targets_from(
            Some(std::path::Path::new(fixture)),
            "p80",
            &["ipc=9".into()],
            &[],
        )
        .unwrap();
        assert_eq!(t.get("ipc"), Some(9.0));
        assert_eq!(t.get("cpu_limit_cores"), Some(8.0));
        assert!(
            targets_from(None, "avg", &[], &[])
                .unwrap_err()
                .to_string()
                .contains("no targets")
        );
        assert!(targets_from(None, "avg", &["nope=1".into()], &[]).is_err());
        assert!(targets_from(None, "avg", &[], &[("nope", 1.0)]).is_err());
        assert!(targets_from(None, "avg", &[], &[("ipc", -1.0)]).is_err());
        assert!(targets_from(Some(std::path::Path::new("/nonexistent")), "avg", &[], &[]).is_err());
        assert!(targets_from(Some(std::path::Path::new(fixture)), "p42", &[], &[]).is_err());
    }

    #[test]
    fn every_field_is_a_named_option() {
        for f in FIELDS {
            let flag = format!("--{}", flag_name(f.key));
            let cli = Cli::try_parse_from(["thrashgen", "run", &flag, "2.5"]).unwrap();
            let Command::Run(args) = cli.command else {
                panic!("not run")
            };
            assert_eq!(args.fields.values, vec![(f.key, 2.5)], "{flag}");
        }
        let cli = Cli::try_parse_from([
            "thrashgen",
            "calibrate",
            "-o",
            "c.json",
            "--ipc",
            "1",
            "--set",
            "ipc=3",
            "--l3-mpki",
            "4",
        ])
        .unwrap();
        let Command::Calibrate(args) = cli.command else {
            panic!("not calibrate")
        };
        let t = targets_from(None, "avg", &args.run.sets, &args.run.fields.values).unwrap();
        assert_eq!(t.get("ipc"), Some(1.0), "a field option wins over --set");
        assert_eq!(t.get("l3_mpki"), Some(4.0));
        assert!(Cli::try_parse_from(["thrashgen", "run", "--ipc", "fast"]).is_err());
        assert!(Cli::try_parse_from(["thrashgen", "run", "--ipc"]).is_err());
        assert_eq!(flag_name("l3_mpki"), "l3-mpki");
    }
}
