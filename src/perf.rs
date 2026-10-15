//! Counters through `perf_event_open`, scoped to our own threads, a
//! task, a CPU, or a cgroup on a CPU, with multiplexing corrected by
//! the enabled and running times the kernel reports.
//!
//! Counters are opened disabled and read individually; group reads are
//! avoided so inherited events on short-lived threads cannot fail a
//! whole set. Scaling makes a read comparable even when the PMU was
//! shared.

use std::fmt;
use std::fs::File;
use std::io;
use std::time::Duration;

use perf_event::events::{Hardware, Raw};
use perf_event::{Builder, Counter};

use crate::events::Encoded;

/// A counter to open: a name for reports and the encoded event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// Name used in readings.
    pub name: String,
    /// Event encoding.
    pub enc: Encoded,
}

impl Spec {
    /// A raw or dynamic-PMU event.
    pub fn new(name: impl Into<String>, enc: Encoded) -> Spec {
        Spec {
            name: name.into(),
            enc,
        }
    }

    /// The generic hardware event the kernel maps to the core's
    /// instruction counter.
    pub fn instructions() -> Spec {
        Spec::hardware("instructions", Hardware::INSTRUCTIONS)
    }

    /// The generic hardware event for core cycles.
    pub fn cycles() -> Spec {
        Spec::hardware("cycles", Hardware::CPU_CYCLES)
    }

    fn hardware(name: &str, hw: Hardware) -> Spec {
        Spec {
            name: name.into(),
            enc: Encoded {
                type_id: 0,
                config: u64::from(hw),
                config1: 0,
                config2: 0,
            },
        }
    }
}

/// Whose activity to count.
#[derive(Clone, Copy, Debug)]
pub enum Target<'a> {
    /// This process: pid 0 on any CPU; with inherit, threads created
    /// after the open are included.
    SelfThreads,
    /// Another task by pid (or tid), on any CPU.
    Task(i32),
    /// Everything on one CPU.
    Cpu(u32),
    /// A cgroup's tasks while they run on one CPU; `cgroup` is an open
    /// handle on the cgroup directory.
    CgroupOnCpu {
        /// Cgroup directory.
        cgroup: &'a File,
        /// CPU.
        cpu: u32,
    },
}

impl fmt::Display for Target<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::SelfThreads => f.write_str("own threads"),
            Target::Task(pid) => write!(f, "task {pid}"),
            Target::Cpu(c) => write!(f, "cpu {c}"),
            Target::CgroupOnCpu { cpu, .. } => write!(f, "cgroup on cpu {cpu}"),
        }
    }
}

/// How to open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Count kernel mode too. Needs `perf_event_paranoid` at most 1 or
    /// `CAP_PERFMON`.
    pub kernel: bool,
    /// Follow children and new threads of the target task.
    pub inherit: bool,
    /// Fail rather than multiplex.
    pub pinned: bool,
}

/// Why an open failed, in terms of what to do about it.
pub fn hint(err: &io::Error) -> &'static str {
    match err.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => {
            "not permitted at this perf_event_paranoid level without CAP_PERFMON"
        }
        Some(libc::ENOENT) => "the PMU or event is not supported here",
        Some(libc::EINVAL) => {
            "the attributes are not valid for this PMU, for example a task-scoped uncore event"
        }
        Some(libc::ENODEV) => "no such CPU",
        Some(libc::EOPNOTSUPP) => "the PMU does not support this mode",
        Some(libc::ESRCH) => "no such task",
        Some(libc::EMFILE) | Some(libc::ENFILE) => "out of file descriptors",
        _ => "unexpected error",
    }
}

/// An open that failed.
#[derive(Debug)]
pub struct OpenError {
    /// Counter name.
    pub name: String,
    /// Target description.
    pub target: String,
    /// The OS error.
    pub source: io::Error,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} on {}: {} ({})",
            self.name,
            self.target,
            self.source,
            hint(&self.source)
        )
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Build the attributes for one counter without opening it.
pub fn builder<'a>(spec: &Spec, target: &Target<'a>, opts: Options) -> Builder<'a> {
    let mut b = Builder::new(
        Raw::new(spec.enc.config)
            .config1(spec.enc.config1)
            .config2(spec.enc.config2),
    );
    b.attrs_mut().type_ = spec.enc.type_id;
    b.enabled(false)
        .exclude_kernel(!opts.kernel)
        .exclude_hv(true)
        .inherit(opts.inherit)
        .pinned(opts.pinned);
    match *target {
        Target::SelfThreads => b.observe_self().any_cpu(),
        Target::Task(pid) => b.observe_pid(pid).any_cpu(),
        Target::Cpu(cpu) => b.any_pid().one_cpu(cpu as usize),
        Target::CgroupOnCpu { cgroup, cpu } => b.observe_cgroup(cgroup).one_cpu(cpu as usize),
    };
    b
}

/// One counter's value at a read.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// Counter name.
    pub name: String,
    /// Count as the kernel reported it.
    pub raw: u64,
    /// Count scaled by enabled over running time, correcting for time
    /// the PMU was multiplexed away. Equals `raw` when never descheduled.
    pub scaled: f64,
    /// Time the counter was enabled.
    pub time_enabled: Duration,
    /// Time it was actually on the PMU.
    pub time_running: Duration,
}

/// Scale a raw count by enabled / running. With no running time there
/// is nothing to scale from, so the raw count is returned.
pub fn scale(raw: u64, enabled: Duration, running: Duration) -> f64 {
    if running.is_zero() || enabled <= running {
        raw as f64
    } else {
        raw as f64 * enabled.as_secs_f64() / running.as_secs_f64()
    }
}

/// A set of open counters on one target.
pub struct Counters {
    items: Vec<(String, Counter)>,
}

impl fmt::Debug for Counters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

impl Counters {
    /// Open every spec on the target. Fails on the first counter that
    /// cannot be opened, naming it.
    pub fn open(specs: &[Spec], target: &Target<'_>, opts: Options) -> Result<Counters, OpenError> {
        let mut items = Vec::with_capacity(specs.len());
        for spec in specs {
            let counter = builder(spec, target, opts)
                .build()
                .map_err(|source| OpenError {
                    name: spec.name.clone(),
                    target: target.to_string(),
                    source,
                })?;
            items.push((spec.name.clone(), counter));
        }
        Ok(Counters { items })
    }

    /// Start counting.
    pub fn enable(&mut self) -> io::Result<()> {
        self.items.iter_mut().try_for_each(|(_, c)| c.enable())
    }

    /// Stop counting.
    pub fn disable(&mut self) -> io::Result<()> {
        self.items.iter_mut().try_for_each(|(_, c)| c.disable())
    }

    /// Zero every counter.
    pub fn reset(&mut self) -> io::Result<()> {
        self.items.iter_mut().try_for_each(|(_, c)| c.reset())
    }

    /// Read every counter.
    pub fn read(&mut self) -> io::Result<Vec<Reading>> {
        self.items
            .iter_mut()
            .map(|(name, c)| {
                let d = c.read_full()?;
                let time_enabled = d.time_enabled().unwrap_or_default();
                let time_running = d.time_running().unwrap_or_default();
                Ok(Reading {
                    name: name.clone(),
                    raw: d.count(),
                    scaled: scale(d.count(), time_enabled, time_running),
                    time_enabled,
                    time_running,
                })
            })
            .collect()
    }

    /// Names in open order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|(n, _)| n.as_str())
    }

    /// Number of counters.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// What actually opens on this host, found by trying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// An instructions counter on our own threads, user mode.
    pub self_user: Result<(), String>,
    /// The same including kernel mode.
    pub self_kernel: Result<(), String>,
    /// An instructions counter on CPU 0 for every task.
    pub per_cpu: Result<(), String>,
}

/// Try the three opens that decide the measurement tier.
pub fn probe() -> Probe {
    let instr = [Spec::instructions()];
    let try_open = |target: Target<'_>, kernel: bool| {
        Counters::open(
            &instr,
            &target,
            Options {
                kernel,
                inherit: false,
                pinned: false,
            },
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    };
    Probe {
        self_user: try_open(Target::SelfThreads, false),
        self_kernel: try_open(Target::SelfThreads, true),
        per_cpu: try_open(Target::Cpu(0), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(type_id: u32, config: u64) -> Encoded {
        Encoded {
            type_id,
            config,
            config1: 0,
            config2: 0,
        }
    }

    #[test]
    fn builder_sets_type_config_scope_and_modes() {
        let spec = Spec::new(
            "x",
            Encoded {
                type_id: 13,
                config: 0x104,
                config1: 7,
                config2: 9,
            },
        );
        let b = builder(
            &spec,
            &Target::SelfThreads,
            Options {
                kernel: false,
                inherit: true,
                pinned: false,
            },
        );
        let a = b.attrs();
        assert_eq!(a.type_, 13);
        assert_eq!((a.config, a.config1, a.config2), (0x104, 7, 9));
        assert_eq!(a.inherit(), 1);
        assert_eq!(a.exclude_kernel(), 1);
        assert_eq!(a.exclude_hv(), 1);
        assert_eq!(a.disabled(), 1);
        assert_eq!(a.pinned(), 0);

        let b = builder(
            &spec,
            &Target::Cpu(3),
            Options {
                kernel: true,
                inherit: false,
                pinned: true,
            },
        );
        let a = b.attrs();
        assert_eq!(a.exclude_kernel(), 0);
        assert_eq!(a.inherit(), 0);
        assert_eq!(a.pinned(), 1);

        let hw = Spec::instructions();
        assert_eq!(hw.enc.type_id, 0, "PERF_TYPE_HARDWARE");
        assert_eq!(hw.enc.config, 1, "PERF_COUNT_HW_INSTRUCTIONS");
        assert_eq!(Spec::cycles().enc.config, 0);
    }

    #[test]
    fn target_names_and_hints() {
        assert_eq!(Target::SelfThreads.to_string(), "own threads");
        assert_eq!(Target::Task(42).to_string(), "task 42");
        assert_eq!(Target::Cpu(5).to_string(), "cpu 5");
        let f = tempfile::tempfile().unwrap();
        assert_eq!(
            Target::CgroupOnCpu { cgroup: &f, cpu: 2 }.to_string(),
            "cgroup on cpu 2"
        );
        assert!(hint(&io::Error::from_raw_os_error(libc::EACCES)).contains("CAP_PERFMON"));
        assert!(hint(&io::Error::from_raw_os_error(libc::ENOENT)).contains("not supported"));
        assert!(hint(&io::Error::from_raw_os_error(libc::EINVAL)).contains("uncore"));
        assert_eq!(hint(&io::Error::other("x")), "unexpected error");
        let e = OpenError {
            name: "i".into(),
            target: "t".into(),
            source: io::Error::from_raw_os_error(libc::ESRCH),
        };
        assert!(e.to_string().starts_with("i on t: "));
        assert!(e.to_string().ends_with("(no such task)"));
        assert!(std::error::Error::source(&e).is_some());
    }

    #[test]
    fn scaling() {
        let s = Duration::from_secs;
        assert_eq!(scale(100, s(2), s(1)), 200.0);
        assert_eq!(scale(100, s(1), s(1)), 100.0);
        assert_eq!(
            scale(100, s(1), s(2)),
            100.0,
            "running beyond enabled is not scaled"
        );
        assert_eq!(scale(0, s(1), s(0)), 0.0);
        assert_eq!(
            scale(5, s(1), s(0)),
            5.0,
            "never ran: nothing to scale from"
        );
    }

    #[test]
    fn opening_an_impossible_event_names_it() {
        // Type 0xffff_fff0 is no PMU; the kernel answers ENOENT.
        let spec = Spec::new("bogus", enc(0xffff_fff0, 0));
        let err = Counters::open(&[spec], &Target::SelfThreads, Options::default()).unwrap_err();
        assert_eq!(err.name, "bogus");
        assert_eq!(err.target, "own threads");
        assert_eq!(err.source.raw_os_error(), Some(libc::ENOENT), "{err}");
        let empty = Counters::open(&[], &Target::SelfThreads, Options::default()).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn probe_runs() {
        let p = probe();
        // Without a PMU every arm fails; with one, the user arm works.
        if p.self_user.is_ok() {
            assert!(
                p.self_kernel.is_ok()
                    || p.self_kernel.as_ref().unwrap_err().contains("CAP_PERFMON")
            );
        }
    }
}

#[cfg(all(test, feature = "hw-tests"))]
mod hw_tests {
    use super::*;
    use crate::cpu;
    use crate::events::{Sysfs, table_for};
    use std::hint::black_box;

    fn burn(iters: u64) -> u64 {
        let mut x = 1u64;
        for i in 0..iters {
            x = black_box(x.wrapping_mul(6364136223846793005).wrapping_add(i));
        }
        x
    }

    #[test]
    fn counts_our_own_work() {
        let specs = [Spec::instructions(), Spec::cycles()];
        let mut c = Counters::open(&specs, &Target::SelfThreads, Options::default()).unwrap();
        c.enable().unwrap();
        burn(20_000_000);
        c.disable().unwrap();
        let r = c.read().unwrap();
        assert_eq!(r.len(), 2);
        assert!(r[0].raw > 20_000_000, "{:?}", r[0]);
        assert!(r[1].raw > 1_000_000, "{:?}", r[1]);
        assert!(r[0].scaled >= r[0].raw as f64);
        assert!(!r[0].time_enabled.is_zero());
        c.reset().unwrap();
        let r = c.read().unwrap();
        assert!(r[0].raw < 100_000, "reset: {:?}", r[0]);
    }

    #[test]
    fn inherit_follows_threads_created_after_the_open() {
        let specs = [Spec::instructions()];
        let open = |inherit| {
            let mut c = Counters::open(
                &specs,
                &Target::SelfThreads,
                Options {
                    inherit,
                    ..Options::default()
                },
            )
            .unwrap();
            c.enable().unwrap();
            std::thread::spawn(|| burn(50_000_000)).join().unwrap();
            c.disable().unwrap();
            c.read().unwrap()[0].raw
        };
        let with = open(true);
        let without = open(false);
        assert!(with > 50_000_000, "inherited count {with}");
        assert!(
            without < with / 5,
            "non-inherited {without} vs inherited {with}"
        );
    }

    #[test]
    fn vendor_events_open_on_this_host() {
        let ident = cpu::detect().unwrap();
        let table = table_for(&ident).unwrap_or_else(|| panic!("no event table for {ident:?}"));
        let sysfs = Sysfs::default();
        let mut specs = vec![Spec::instructions()];
        for s in table
            .dtlb
            .iter()
            .copied()
            .chain([table.itlb])
            .chain(table.l3_miss_proxy)
        {
            let def = s.def();
            specs.push(Spec::new(
                def.name,
                sysfs.encode(table.vendor, &def).unwrap(),
            ));
        }
        if table.l3_miss.def().pmu == crate::events::CORE {
            let def = table.l3_miss.def();
            specs.push(Spec::new(
                def.name,
                sysfs.encode(table.vendor, &def).unwrap(),
            ));
        }
        let mut c = Counters::open(
            &specs,
            &Target::SelfThreads,
            Options {
                inherit: true,
                ..Options::default()
            },
        )
        .unwrap();
        c.enable().unwrap();
        let v: Vec<u64> = (0..4_000_000).collect();
        black_box(v.iter().sum::<u64>());
        c.disable().unwrap();
        let r = c.read().unwrap();
        assert!(r[0].raw > 1_000_000);
        for x in &r[1..] {
            assert!(!x.time_enabled.is_zero(), "{x:?}");
        }
    }

    #[test]
    fn probe_reports_user_counters_available() {
        assert_eq!(probe().self_user, Ok(()));
    }
}

#[cfg(all(test, feature = "root-tests"))]
mod root_tests {
    use super::*;

    #[test]
    fn per_cpu_counters_open_with_perfmon() {
        let p = probe();
        assert_eq!(p.per_cpu, Ok(()));
        assert_eq!(p.self_kernel, Ok(()));
        let mut c = Counters::open(
            &[Spec::cycles()],
            &Target::Cpu(0),
            Options {
                kernel: true,
                ..Options::default()
            },
        )
        .unwrap();
        c.enable().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let r = c.read().unwrap();
        assert!(r[0].raw > 0, "{:?}", r[0]);
    }
}
