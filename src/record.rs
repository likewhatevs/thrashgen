//! The `record` command: sample a cgroup, a process, a command or the
//! host and write a recording.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use nix::sys::ptrace;
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid};
use nix::unistd::Pid;

use crate::caps::{Expectations, Facts, Root};
use crate::cgroup::Cgroup;
use crate::cpu;
use crate::events::{Sysfs, table_for};
use crate::measure::Scope;
use crate::perf;
use crate::plan::{Access, ScopeInfo, plan};
use crate::resctrl::{MonGroup, Resctrl};
use crate::sampler::{LiveCounters, Sampler};
use crate::samples::{self, FORMAT_VERSION, Header, Record, write_record};
use crate::topology::Topology;

/// What to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    /// A cgroup v2 path relative to the hierarchy root.
    Cgroup(String),
    /// An existing process.
    Pid(i32),
    /// The whole host.
    Host,
    /// A command to launch and follow until it exits.
    Command(Vec<String>),
}

/// Timing of a recording.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// Spacing of counter and memory samples, seconds.
    pub interval_s: f64,
    /// Spacing of busy-cores samples, seconds.
    pub cpu_interval_s: f64,
    /// Stop after this long; `None` runs until interrupted or, for a
    /// command, until it exits.
    pub duration_s: Option<f64>,
    /// Send a recorded command's stdout to stderr, so it cannot mix
    /// with a recording written to stdout.
    pub child_stdout_to_stderr: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            interval_s: 60.0,
            cpu_interval_s: 1.0,
            duration_s: None,
            child_stdout_to_stderr: false,
        }
    }
}

/// What a finished recording did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Counter and memory samples written.
    pub samples: usize,
    /// Busy-cores samples written.
    pub cpu_samples: usize,
    /// Fallbacks and skipped tasks, for the user.
    pub notes: Vec<String>,
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Whether SIGINT or SIGTERM has arrived since the handler was installed.
pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Make SIGINT and SIGTERM end the recording cleanly.
pub fn install_stop_handler() -> Result<()> {
    for sig in [Signal::SIGINT, Signal::SIGTERM] {
        // SAFETY: the handler only stores to an atomic.
        unsafe { signal::signal(sig, SigHandler::Handler(on_signal)) }
            .with_context(|| format!("install {sig} handler"))?;
    }
    Ok(())
}

/// Thread ids of a process, from `/proc/<pid>/task`.
pub fn thread_ids(root: &Root, pid: i32) -> io::Result<Vec<i32>> {
    let mut tids: Vec<i32> = std::fs::read_dir(root.path(format!("/proc/{pid}/task")))?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    tids.sort_unstable();
    Ok(tids)
}

/// The planning facts for a cgroup: its effective CPUs limited to the
/// online ones, and its current threads.
pub fn cgroup_scope_info(cg: &Cgroup, topo: &Topology) -> io::Result<ScopeInfo> {
    let online: Vec<u32> = topo.cpus.iter().map(|c| c.id).collect();
    let cpus = match cg.cpuset_effective()? {
        Some(set) => online.iter().copied().filter(|c| set.contains(c)).collect(),
        None => online,
    };
    let tids = cg.threads()?.into_iter().map(|t| t as i32).collect();
    Ok(ScopeInfo::Cgroup { cpus, tids })
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A command launched and held at its exec, so counters attach before
/// any of its code runs.
///
/// `std::process::Command::spawn` waits for the exec, so the child
/// cannot stop itself beforehand without deadlocking the parent.
/// Instead the child asks to be traced by us: the kernel stops it with
/// SIGTRAP right after a successful exec, and detaching lets it run.
/// Where tracing is refused, for example by Yama, the child starts at
/// once and counters attach a moment late, which the notes record.
struct StoppedChild {
    child: Child,
    held: bool,
}

impl StoppedChild {
    fn spawn(
        argv: &[String],
        stdout_to_stderr: bool,
        notes: &mut Vec<String>,
    ) -> Result<StoppedChild> {
        let (program, args) = argv.split_first().ok_or_else(|| anyhow!("empty command"))?;
        let build = |trace: bool| -> io::Result<Child> {
            let mut cmd = Command::new(program);
            cmd.args(args);
            if stdout_to_stderr {
                cmd.stdout(Stdio::from(io::stderr().as_fd().try_clone_to_owned()?));
            }
            if trace {
                // SAFETY: traceme is one ptrace syscall, async-signal-safe
                // and allocation-free.
                unsafe {
                    cmd.pre_exec(|| ptrace::traceme().map_err(io::Error::from));
                }
            }
            cmd.spawn()
        };
        match build(true) {
            Ok(child) => {
                let pid = Pid::from_raw(child.id() as i32);
                match waitpid(pid, None).context("wait for the command to stop at exec")? {
                    WaitStatus::Stopped(_, Signal::SIGTRAP) => {
                        Ok(StoppedChild { child, held: true })
                    }
                    other => bail!("command did not stop at exec: {other:?}"),
                }
            }
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
                notes.push(
                    "tracing the command is not permitted: counters attached after it started"
                        .into(),
                );
                let child = build(false).with_context(|| format!("spawn {program}"))?;
                Ok(StoppedChild { child, held: false })
            }
            Err(e) => Err(e).with_context(|| format!("spawn {program}")),
        }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// Let the command run.
    fn resume(&mut self) -> Result<()> {
        if self.held {
            ptrace::detach(Pid::from_raw(self.pid()), None).context("detach from the command")?;
            self.held = false;
        }
        Ok(())
    }

    /// Whether the command has exited. It is not reaped here, so its
    /// final CPU time stays readable in `/proc` for the last sample.
    fn finished(&self) -> bool {
        let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
        !matches!(
            waitid(Id::Pid(Pid::from_raw(self.pid())), flags),
            Ok(WaitStatus::StillAlive)
        )
    }

    fn terminate(&mut self) {
        let _ = signal::kill(Pid::from_raw(self.pid()), Signal::SIGTERM);
        self.reap();
    }

    fn reap(&mut self) {
        let _ = self.child.wait();
    }
}

/// A scope that no longer exists: its cgroup was removed or its process
/// exited, so its files are gone. The recording ends there.
fn scope_gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

/// Record `what` with `settings`, writing JSON lines to `out`.
pub fn run(what: &What, settings: &Settings, out: &mut dyn Write) -> Result<Summary> {
    if settings.interval_s <= 0.0 || settings.cpu_interval_s <= 0.0 {
        bail!("intervals must be positive");
    }
    let root = Root::default();
    let facts = Facts::detect(&root);
    let rules = Expectations::from_facts(&facts);
    let access = Access::new(&rules, &perf::probe());
    let ident = cpu::detect();
    let table = ident.as_ref().and_then(table_for);
    let sysfs = Sysfs::default();
    let topo = Topology::read(&sysfs).context("read CPU topology")?;
    let online: Vec<u32> = topo.cpus.iter().map(|c| c.id).collect();
    let mut summary = Summary::default();

    let mut child = None;
    let mut cgroup_file = None;
    let (scope, info, target) = match what {
        What::Cgroup(path) => {
            let mount = facts
                .cgroup
                .as_ref()
                .map(|c| c.mount.clone())
                .ok_or_else(|| anyhow!("no cgroup v2 mount"))?;
            let cg = Cgroup::new(&root, &mount, path);
            if !cg.exists() {
                bail!("cgroup {} does not exist under {}", path, mount.display());
            }
            let info =
                cgroup_scope_info(&cg, &topo).with_context(|| format!("read cgroup {path}"))?;
            cgroup_file =
                Some(File::open(cg.dir()).with_context(|| format!("open cgroup {path}"))?);
            (
                Scope::Cgroup(cg),
                info,
                samples::Target::Cgroup {
                    path: path.trim_matches('/').to_string(),
                },
            )
        }
        What::Pid(pid) => {
            let tids = thread_ids(&root, *pid).with_context(|| format!("list threads of {pid}"))?;
            (
                Scope::Process(*pid),
                ScopeInfo::Process { tids },
                samples::Target::Process { pid: *pid as u32 },
            )
        }
        What::Host => (
            Scope::Host,
            ScopeInfo::Host { cpus: online },
            samples::Target::Host,
        ),
        What::Command(argv) => {
            let stopped =
                StoppedChild::spawn(argv, settings.child_stdout_to_stderr, &mut summary.notes)?;
            let pid = stopped.pid();
            child = Some(stopped);
            (
                Scope::Process(pid),
                ScopeInfo::Process { tids: vec![pid] },
                samples::Target::Command { argv: argv.clone() },
            )
        }
    };

    let bandwidth = match Resctrl::from_facts(&root, &facts) {
        None => {
            summary.notes.push(
                "DRAM bandwidth not measured: resctrl is not mounted with mbm_total_bytes".into(),
            );
            None
        }
        Some(rc) if matches!(what, What::Host) => Some(rc.root_group()),
        Some(rc) => {
            let tids: Vec<i32> = match &info {
                ScopeInfo::Cgroup { tids, .. } | ScopeInfo::Process { tids } => tids.clone(),
                _ => Vec::new(),
            };
            match rc.create_mon_group(&format!("thrashgen-{}", std::process::id())) {
                Ok(group) => match group.add_tasks(&tids) {
                    Ok(_) => {
                        if matches!(what, What::Cgroup(_)) {
                            summary.notes.push("DRAM bandwidth covers the cgroup's current tasks and their descendants; tasks that join later are not counted".into());
                        }
                        Some(group)
                    }
                    Err(e) => {
                        summary.notes.push(format!("DRAM bandwidth not measured: cannot add tasks to the resctrl group: {e}"));
                        None
                    }
                },
                Err(e) => {
                    summary.notes.push(format!(
                        "DRAM bandwidth not measured: cannot create a resctrl group: {e}"
                    ));
                    None
                }
            }
        }
    };

    let plan = plan(&info, &access, table, &sysfs, &topo).context("plan counters")?;
    summary.notes.extend(plan.notes.iter().cloned());
    let (mut live, skipped) =
        LiveCounters::open_with(&plan, cgroup_file.as_ref(), true).context("open counters")?;
    summary.notes.extend(skipped);
    live.enable().context("enable counters")?;
    let counters = if live.is_empty() { None } else { Some(live) };

    let mut sampler: Sampler<LiveCounters, MonGroup> = Sampler::new(
        scope,
        root,
        plan.fields.clone(),
        counters,
        bandwidth,
        vec![],
    );
    let header = Header {
        version: FORMAT_VERSION,
        host: nix::unistd::gethostname()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default(),
        kernel: nix::sys::utsname::uname()
            .map(|u| u.release().to_string_lossy().into_owned())
            .unwrap_or_default(),
        cpu: ident,
        started_unix_s: unix_now(),
        interval_s: settings.interval_s,
        cpu_interval_s: settings.cpu_interval_s,
        target,
        provenance: sampler.provenance(),
    };
    write_record(&mut *out, &Record::Header(header))?;
    out.flush()?;

    let start = Instant::now();
    let t0 = unix_now();
    sampler.start(t0).context("initial readings")?;
    if let Some(c) = &mut child {
        c.resume()?;
    }
    let cpu_step = Duration::from_secs_f64(settings.cpu_interval_s);
    let step = Duration::from_secs_f64(settings.interval_s);
    let deadline = settings.duration_s.map(Duration::from_secs_f64);
    let (mut next_cpu, mut next_interval) = (cpu_step, step);
    let mut last_interval_t = t0;
    loop {
        let next = next_cpu.min(next_interval);
        while start.elapsed() < next {
            if STOP.load(Ordering::SeqCst) || child.as_ref().is_some_and(StoppedChild::finished) {
                break;
            }
            std::thread::sleep((next - start.elapsed()).min(Duration::from_millis(100)));
        }
        let stop = STOP.load(Ordering::SeqCst)
            || child.as_ref().is_some_and(StoppedChild::finished)
            || deadline.is_some_and(|d| start.elapsed() >= d);
        let t = unix_now();
        if stop {
            if t > last_interval_t + settings.cpu_interval_s / 2.0 {
                match (sampler.cpu_tick(t), sampler.interval_tick(t)) {
                    (Ok(c), Ok(s)) => {
                        write_record(&mut *out, &Record::Cpu(c))?;
                        summary.cpu_samples += 1;
                        write_record(&mut *out, &Record::Sample(s))?;
                        summary.samples += 1;
                    }
                    (Err(e), _) | (_, Err(e)) if scope_gone(&e) => {
                        summary
                            .notes
                            .push("the scope vanished before the final sample".into());
                    }
                    (Err(e), _) => return Err(e).context("busy-cores sample"),
                    (_, Err(e)) => return Err(e).context("interval sample"),
                }
            }
            out.flush()?;
            break;
        }
        if start.elapsed() >= next_cpu {
            match sampler.cpu_tick(t) {
                Ok(c) => {
                    write_record(&mut *out, &Record::Cpu(c))?;
                    summary.cpu_samples += 1;
                }
                Err(e) if scope_gone(&e) => {
                    summary
                        .notes
                        .push("the scope vanished; recording ended".into());
                    out.flush()?;
                    break;
                }
                Err(e) => return Err(e).context("busy-cores sample"),
            }
            next_cpu += cpu_step;
        }
        if start.elapsed() >= next_interval {
            match sampler.interval_tick(t) {
                Ok(s) => {
                    write_record(&mut *out, &Record::Sample(s))?;
                    summary.samples += 1;
                }
                Err(e) if scope_gone(&e) => {
                    summary
                        .notes
                        .push("the scope vanished; recording ended".into());
                    out.flush()?;
                    break;
                }
                Err(e) => return Err(e).context("interval sample"),
            }
            last_interval_t = t;
            next_interval += step;
        }
        out.flush()?;
    }
    if let Some(mut c) = child {
        if c.finished() {
            c.reap();
        } else {
            c.terminate();
            summary
                .notes
                .push("command was still running at the deadline and was terminated".into());
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture_root() -> Root {
        Root::at(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/root"))
    }

    #[test]
    fn thread_ids_from_proc() {
        assert_eq!(thread_ids(&fixture_root(), 4242).unwrap(), [4242, 4243]);
        assert!(thread_ids(&fixture_root(), 1).is_err());
        let me = std::process::id() as i32;
        assert!(thread_ids(&Root::default(), me).unwrap().contains(&me));
    }

    #[test]
    fn cgroup_scope_info_limits_cpus_to_online() {
        let root = fixture_root();
        let topo = Topology::read(&Sysfs::at(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/root"
        )))
        .unwrap();
        let cg = Cgroup::new(
            &root,
            Path::new("/sys/fs/cgroup"),
            "user.slice/user-1000.slice/session-9.scope",
        );
        let info = cgroup_scope_info(&cg, &topo).unwrap();
        assert_eq!(
            info,
            ScopeInfo::Cgroup {
                cpus: vec![0, 1, 8, 32, 33],
                tids: vec![4242, 4243, 4300]
            }
        );

        let dir = tempfile::tempdir().unwrap();
        let r = Root::at(dir.path());
        let cg = Cgroup::new(&r, Path::new("/sys/fs/cgroup"), "x");
        std::fs::create_dir_all(cg.dir()).unwrap();
        std::fs::write(cg.dir().join("cgroup.threads"), "9\n").unwrap();
        std::fs::write(cg.dir().join("cpuset.cpus.effective"), "1,33\n").unwrap();
        assert_eq!(
            cgroup_scope_info(&cg, &topo).unwrap(),
            ScopeInfo::Cgroup {
                cpus: vec![1, 33],
                tids: vec![9]
            }
        );
        std::fs::remove_file(cg.dir().join("cgroup.threads")).unwrap();
        assert!(cgroup_scope_info(&cg, &topo).is_err());
    }

    #[test]
    fn settings_are_validated() {
        let mut out = Vec::new();
        let bad = Settings {
            interval_s: 0.0,
            ..Settings::default()
        };
        assert!(
            run(&What::Host, &bad, &mut out)
                .unwrap_err()
                .to_string()
                .contains("positive")
        );
        assert!(
            run(&What::Command(vec![]), &Settings::default(), &mut out)
                .unwrap_err()
                .to_string()
                .contains("empty command")
        );
        let exec = run(
            &What::Command(vec!["/nonexistent/program".into()]),
            &Settings::default(),
            &mut out,
        )
        .unwrap_err();
        assert!(
            exec.to_string().contains("spawn /nonexistent/program"),
            "{exec}"
        );
        assert!(
            run(&What::Pid(i32::MAX - 1), &Settings::default(), &mut out)
                .unwrap_err()
                .to_string()
                .contains("list threads")
        );
        assert!(
            run(
                &What::Cgroup("no/such/cgroup".into()),
                &Settings::default(),
                &mut out
            )
            .unwrap_err()
            .to_string()
            .contains("does not exist")
        );
        assert!(unix_now() > 1.6e9);
    }

    #[test]
    fn records_a_command_until_it_exits() {
        let mut out = Vec::new();
        let settings = Settings {
            interval_s: 0.4,
            cpu_interval_s: 0.1,
            duration_s: Some(30.0),
            child_stdout_to_stderr: false,
        };
        let what = What::Command(vec![
            "sh".into(),
            "-c".into(),
            "i=0; while [ $i -lt 300000 ]; do i=$((i+1)); done".into(),
        ]);
        let summary = run(&what, &settings, &mut out).unwrap();
        let rec = samples::Recording::read(out.as_slice()).unwrap();
        assert_eq!(
            rec.header.target,
            samples::Target::Command {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "i=0; while [ $i -lt 300000 ]; do i=$((i+1)); done".into()
                ]
            }
        );
        assert_eq!(rec.header.interval_s, 0.4);
        assert!(rec.header.provenance.contains_key("cpu_usage_cores"));
        assert_eq!(rec.cpu.len(), summary.cpu_samples);
        assert_eq!(rec.samples.len(), summary.samples);
        assert!(
            !rec.samples.is_empty(),
            "the final interval is always written"
        );
        let busy: f64 = rec.cpu.iter().map(|c| c.cores).sum::<f64>();
        assert!(busy > 0.0, "the shell loop burned CPU: {:?}", rec.cpu);
        assert!(rec.samples.iter().all(|s| s.scope.starts_with("pid ")));
        assert!(!summary.notes.iter().any(|n| n.contains("terminated")));
    }

    #[test]
    fn records_the_host_for_a_fixed_duration() {
        let mut out = Vec::new();
        let settings = Settings {
            interval_s: 0.3,
            cpu_interval_s: 0.1,
            duration_s: Some(0.35),
            child_stdout_to_stderr: false,
        };
        let summary = run(&What::Host, &settings, &mut out).unwrap();
        let rec = samples::Recording::read(out.as_slice()).unwrap();
        assert_eq!(rec.header.target, samples::Target::Host);
        assert!(summary.cpu_samples >= 3, "{summary:?}");
        assert_eq!(summary.samples, rec.samples.len());
        assert!(
            rec.samples
                .iter()
                .all(|s| s.values.contains_key("memory_current_bytes"))
        );
        assert!(rec.cpu.iter().all(|c| c.scope == "host"));
        let bw_note = summary.notes.iter().any(|n| n.contains("DRAM bandwidth"));
        assert_eq!(
            bw_note,
            !rec.header.provenance.contains_key("dram_bytes_per_sec"),
            "{summary:?}"
        );
    }

    #[test]
    fn a_process_that_exits_ends_the_recording_with_a_note() {
        // Reap the child as soon as it exits; a zombie keeps its /proc
        // entry and the recording would see it as alive but idle.
        let mut short = std::process::Command::new("sleep")
            .arg("0.3")
            .spawn()
            .unwrap();
        let pid = short.id() as i32;
        let reaper = std::thread::spawn(move || short.wait());
        let mut out = Vec::new();
        let settings = Settings {
            interval_s: 0.2,
            cpu_interval_s: 0.1,
            duration_s: Some(3.0),
            child_stdout_to_stderr: false,
        };
        let summary = run(&What::Pid(pid), &settings, &mut out).unwrap();
        reaper.join().unwrap().unwrap();
        assert!(
            summary.notes.iter().any(|n| n.contains("vanished")),
            "{summary:?}"
        );
        let rec = samples::Recording::read(out.as_slice()).unwrap();
        assert!(
            rec.cpu.len() >= 2 && rec.cpu.len() < 20,
            "{}",
            rec.cpu.len()
        );
    }

    #[test]
    fn records_a_pid_and_terminates_a_command_at_the_deadline() {
        let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
        let sleeper_pid = sleeper.id();
        let mut out = Vec::new();
        let settings = Settings {
            interval_s: 0.2,
            cpu_interval_s: 0.1,
            duration_s: Some(0.25),
            child_stdout_to_stderr: false,
        };
        let summary = run(&What::Pid(sleeper.id() as i32), &settings, &mut out).unwrap();
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        let rec = samples::Recording::read(out.as_slice()).unwrap();
        assert_eq!(
            rec.header.target,
            samples::Target::Process { pid: sleeper_pid }
        );
        assert!(summary.samples >= 1);

        let mut out = Vec::new();
        let settings = Settings {
            interval_s: 0.2,
            cpu_interval_s: 0.1,
            duration_s: Some(0.25),
            child_stdout_to_stderr: false,
        };
        let summary = run(
            &What::Command(vec!["sleep".into(), "30".into()]),
            &settings,
            &mut out,
        )
        .unwrap();
        assert!(
            summary.notes.iter().any(|n| n.contains("terminated")),
            "{summary:?}"
        );
    }
}
