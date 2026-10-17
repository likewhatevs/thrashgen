//! Turn readers and counters into recording samples. Time is passed
//! in, never read, so the sampler is tested with a fake clock and fake
//! counters; `record` drives it with the real ones.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;

use crate::caps::Root;
use crate::measure::{self, BusyTime, Memory, Scope};
use crate::perf::{Counters, OpenError, Options, Reading, Target};
use crate::plan::{FieldSource, Plan, TargetPlan};
use crate::profile::{self, FIELDS, Method, Mode, Provenance};
use crate::samples::{CpuSample, Sample};

/// Anything that yields counter readings, summed by name.
pub trait CounterSource {
    /// Read every counter; names may repeat across groups and are
    /// summed by the sampler.
    fn read(&mut self) -> io::Result<Vec<Reading>>;
}

/// Counters opened from a [`Plan`].
#[derive(Debug)]
pub struct LiveCounters {
    groups: Vec<Counters>,
}

impl LiveCounters {
    /// Open every group. `cgroup` is the cgroup directory for
    /// [`TargetPlan::CgroupOnCpu`] groups.
    pub fn open(plan: &Plan, cgroup: Option<&File>) -> Result<LiveCounters, OpenError> {
        Self::open_with(plan, cgroup, false).map(|(live, _)| live)
    }

    /// Like [`open`](Self::open), but when `lenient` a task that has
    /// exited between planning and opening is skipped and named in the
    /// returned notes instead of failing the whole set.
    pub fn open_with(
        plan: &Plan,
        cgroup: Option<&File>,
        lenient: bool,
    ) -> Result<(LiveCounters, Vec<String>), OpenError> {
        let mut groups = Vec::with_capacity(plan.groups.len());
        let mut notes = Vec::new();
        for g in &plan.groups {
            let target = match g.target {
                TargetPlan::SelfThreads => Target::SelfThreads,
                TargetPlan::Task(tid) => Target::Task(tid),
                TargetPlan::Cpu(cpu) => Target::Cpu(cpu),
                TargetPlan::CgroupOnCpu(cpu) => Target::CgroupOnCpu {
                    cgroup: cgroup.ok_or_else(|| OpenError {
                        name: g.specs.first().map(|s| s.name.clone()).unwrap_or_default(),
                        target: format!("cgroup on cpu {cpu}"),
                        source: io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "no cgroup directory given",
                        ),
                    })?,
                    cpu,
                },
            };
            let opts = Options {
                kernel: g.opts.kernel,
                inherit: g.opts.inherit,
                pinned: g.opts.pinned,
            };
            match Counters::open(&g.specs, &target, opts) {
                Ok(c) => groups.push(c),
                Err(e)
                    if lenient
                        && matches!(g.target, TargetPlan::Task(_))
                        && e.source.raw_os_error() == Some(libc::ESRCH) =>
                {
                    notes.push(format!("{target}: exited before counters were attached"));
                }
                Err(e) => return Err(e),
            }
        }
        Ok((LiveCounters { groups }, notes))
    }

    /// Start counting.
    pub fn enable(&mut self) -> io::Result<()> {
        self.groups.iter_mut().try_for_each(Counters::enable)
    }

    /// Number of groups.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether nothing was opened.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

impl CounterSource for LiveCounters {
    fn read(&mut self) -> io::Result<Vec<Reading>> {
        let mut all = Vec::new();
        for g in &mut self.groups {
            all.extend(g.read()?);
        }
        Ok(all)
    }
}

/// Cumulative bytes from a bandwidth monitor, such as resctrl.
pub trait BandwidthSource {
    /// Total bytes so far, or `None` when the monitor has no reading
    /// yet; an interval with no reading at either end has no rate.
    fn read_bytes(&mut self) -> io::Result<Option<u64>>;
}

/// Sum scaled readings by counter name.
fn sum_by_name(readings: &[Reading]) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for r in readings {
        *out.entry(r.name.clone()).or_insert(0.0) += r.scaled;
    }
    out
}

/// A point the sampler differences against.
#[derive(Clone, Debug)]
struct Mark {
    t: f64,
    busy: BusyTime,
    counters: BTreeMap<String, f64>,
    bytes: Option<u64>,
}

/// Builds [`CpuSample`]s and [`Sample`]s for one scope.
pub struct Sampler<C, B> {
    scope: Scope,
    root: Root,
    fields: BTreeMap<String, FieldSource>,
    counters: Option<C>,
    bandwidth: Option<B>,
    cache_files: Vec<File>,
    cpu_mark: Option<Mark>,
    interval_mark: Option<Mark>,
    mem_sum: [f64; 4],
    mem_n: [u32; 4],
    mem_method: (Method, Method),
}

/// Fields of a [`Sample`] that come from memory reads, in the order
/// the sampler accumulates them.
const MEMORY_KEYS: [&str; 4] = [
    "memory_current_bytes",
    "anon_bytes",
    "pagecache_bytes",
    "shmem_bytes",
];

impl<C: CounterSource, B: BandwidthSource> Sampler<C, B> {
    /// A sampler over a scope. `fields` maps sample keys to the
    /// counters that feed them; `cache_files` are the files whose
    /// residency counts as the process's page cache.
    pub fn new(
        scope: Scope,
        root: Root,
        fields: BTreeMap<String, FieldSource>,
        counters: Option<C>,
        bandwidth: Option<B>,
        cache_files: Vec<File>,
    ) -> Self {
        Sampler {
            scope,
            root,
            fields,
            counters,
            bandwidth,
            cache_files,
            cpu_mark: None,
            interval_mark: None,
            mem_sum: [0.0; 4],
            mem_n: [0; 4],
            mem_method: (Method::Proc, Method::Proc),
        }
    }

    /// The scope being sampled.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    fn mark(&mut self, t: f64) -> io::Result<Mark> {
        let busy = measure::busy_time(&self.scope, &self.root)?;
        let counters = match &mut self.counters {
            Some(c) => sum_by_name(&c.read()?),
            None => BTreeMap::new(),
        };
        let bytes = match &mut self.bandwidth {
            Some(b) => b.read_bytes()?,
            None => None,
        };
        Ok(Mark {
            t,
            busy,
            counters,
            bytes,
        })
    }

    /// Take the initial readings at time `t`.
    pub fn start(&mut self, t: f64) -> io::Result<()> {
        let m = self.mark(t)?;
        self.cpu_mark = Some(m.clone());
        self.interval_mark = Some(m);
        self.mem_sum = [0.0; 4];
        self.mem_n = [0; 4];
        Ok(())
    }

    fn accumulate_memory(&mut self, m: &Memory) {
        self.mem_method = (m.method, m.pagecache_method);
        for (i, v) in [m.current, m.anon, m.pagecache, m.shmem]
            .into_iter()
            .enumerate()
        {
            if let Some(v) = v {
                self.mem_sum[i] += v as f64;
                self.mem_n[i] += 1;
            }
        }
    }

    /// Busy cores since the last CPU tick, and one memory read folded
    /// into the running interval.
    pub fn cpu_tick(&mut self, t: f64) -> io::Result<CpuSample> {
        let prev = self
            .cpu_mark
            .as_ref()
            .ok_or_else(|| io::Error::other("sampler not started"))?;
        let busy = measure::busy_time(&self.scope, &self.root)?;
        let dt = t - prev.t;
        let cores = if dt > 0.0 {
            (busy.total.saturating_sub(prev.busy.total)).as_secs_f64() / dt
        } else {
            0.0
        };
        self.cpu_mark = Some(Mark {
            t,
            busy,
            counters: BTreeMap::new(),
            bytes: None,
        });
        let files: Vec<&File> = self.cache_files.iter().collect();
        let mem = measure::memory(&self.scope, &self.root, &files)?;
        self.accumulate_memory(&mem);
        Ok(CpuSample {
            t,
            scope: self.scope.name(),
            cores,
        })
    }

    /// Rates since the last interval tick, memory averaged over the
    /// CPU ticks in between, and the scope's limits.
    pub fn interval_tick(&mut self, t: f64) -> io::Result<Sample> {
        let prev = self
            .interval_mark
            .take()
            .ok_or_else(|| io::Error::other("sampler not started"))?;
        let now = self.mark(t)?;
        let dt = (t - prev.t).max(f64::EPSILON);
        let mut values = BTreeMap::new();
        values.insert(
            "cpu_usage_cores".to_string(),
            now.busy.total.saturating_sub(prev.busy.total).as_secs_f64() / dt,
        );
        for (key, src) in &self.fields {
            let delta: f64 = src
                .counters
                .iter()
                .map(|n| {
                    now.counters.get(n).copied().unwrap_or(0.0)
                        - prev.counters.get(n).copied().unwrap_or(0.0)
                })
                .sum();
            values.insert(key.clone(), delta.max(0.0) / dt);
        }
        if let (Some(a), Some(b)) = (prev.bytes, now.bytes) {
            values.insert(
                "dram_bytes_per_sec".to_string(),
                b.saturating_sub(a) as f64 / dt,
            );
        }
        // A window shorter than a CPU tick has no memory reads yet; take
        // one now so memory is never missing from a sample.
        if self.mem_n.iter().all(|n| *n == 0) {
            let files: Vec<&File> = self.cache_files.iter().collect();
            let mem = measure::memory(&self.scope, &self.root, &files)?;
            self.accumulate_memory(&mem);
        }
        for (i, key) in MEMORY_KEYS.iter().enumerate() {
            if self.mem_n[i] > 0 {
                values.insert(
                    (*key).to_string(),
                    self.mem_sum[i] / f64::from(self.mem_n[i]),
                );
            }
        }
        let limits = measure::limits(&self.scope);
        if let Some(c) = limits.cpu_cores {
            values.insert("cpu_limit_cores".to_string(), c);
        }
        if let Some(m) = limits.memory_bytes {
            values.insert("memory_limit_bytes".to_string(), m as f64);
        }
        self.mem_sum = [0.0; 4];
        self.mem_n = [0; 4];
        self.interval_mark = Some(now);
        Ok(Sample {
            t,
            scope: self.scope.name(),
            dt: t - prev.t,
            values,
        })
    }

    /// Provenance of every field this sampler can produce.
    pub fn provenance(&self) -> BTreeMap<String, Provenance> {
        let kind = self.scope.kind();
        let busy_method = match self.scope {
            Scope::SelfProcess => Method::Rusage,
            Scope::Cgroup(_) => Method::Cgroup,
            Scope::Process(_) | Scope::Host => Method::Proc,
        };
        let mut out = BTreeMap::new();
        let na = |method| Provenance {
            scope: kind,
            mode: Mode::NotApplicable,
            method,
        };
        out.insert("cpu_usage_cores".to_string(), na(busy_method));
        out.insert("cpu_usage_cv_1s".to_string(), na(busy_method));
        let (mem, cache) = match self.scope {
            Scope::SelfProcess => (Method::Proc, Method::Cachestat),
            Scope::Cgroup(_) => (Method::Cgroup, Method::Cgroup),
            Scope::Process(_) | Scope::Host => (Method::Proc, Method::Proc),
        };
        for key in MEMORY_KEYS {
            out.insert(
                key.to_string(),
                na(if key == "pagecache_bytes" { cache } else { mem }),
            );
        }
        if let Scope::Cgroup(_) = self.scope {
            out.insert("cpu_limit_cores".to_string(), na(Method::Cgroup));
            out.insert("memory_limit_bytes".to_string(), na(Method::Cgroup));
        }
        if self.bandwidth.is_some() {
            out.insert("dram_bytes_per_sec".to_string(), na(Method::Resctrl));
        }
        for (key, src) in &self.fields {
            out.insert(key.clone(), src.provenance);
        }
        for f in FIELDS {
            if let profile::Agg::RatioOfSums { num, .. } = f.agg {
                if let Some(p) = out.get(num).copied() {
                    out.insert(f.key.to_string(), p);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::Cgroup;
    use crate::profile::Mode;
    use std::path::Path;
    use std::time::Duration;

    struct Fake {
        calls: u32,
        per_call: Vec<(&'static str, f64)>,
    }

    impl CounterSource for Fake {
        fn read(&mut self) -> io::Result<Vec<Reading>> {
            self.calls += 1;
            Ok(self
                .per_call
                .iter()
                .map(|(n, step)| Reading {
                    name: (*n).into(),
                    raw: 0,
                    scaled: step * f64::from(self.calls),
                    time_enabled: Duration::from_secs(1),
                    time_running: Duration::from_secs(1),
                })
                .collect())
        }
    }

    struct Bytes(u64);
    impl BandwidthSource for Bytes {
        fn read_bytes(&mut self) -> io::Result<Option<u64>> {
            self.0 += 1000;
            Ok(Some(self.0))
        }
    }

    struct NoBw;
    impl BandwidthSource for NoBw {
        fn read_bytes(&mut self) -> io::Result<Option<u64>> {
            unreachable!()
        }
    }

    fn fixture_root() -> Root {
        Root::at(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/root"))
    }

    fn session(root: &Root) -> Scope {
        Scope::Cgroup(Cgroup::new(
            root,
            Path::new("/sys/fs/cgroup"),
            "user.slice/user-1000.slice/session-9.scope",
        ))
    }

    fn fields() -> BTreeMap<String, FieldSource> {
        let p = |method| Provenance {
            scope: profile::Scope::Cgroup,
            mode: Mode::All,
            method,
        };
        BTreeMap::from([
            (
                "instructions_per_sec".to_string(),
                FieldSource {
                    counters: vec!["instructions".into()],
                    provenance: p(Method::Perf),
                },
            ),
            (
                "dtlb_misses_per_sec".to_string(),
                FieldSource {
                    counters: vec!["dtlb:a".into(), "dtlb:b".into()],
                    provenance: p(Method::Perf),
                },
            ),
            (
                "l3_misses_per_sec".to_string(),
                FieldSource {
                    counters: vec!["l3".into()],
                    provenance: p(Method::Proxy),
                },
            ),
        ])
    }

    #[test]
    fn interval_rates_sum_counters_and_average_memory() {
        let root = fixture_root();
        // Two groups report "instructions"; dtlb has two events; l3 is never reported.
        let fake = Fake {
            calls: 0,
            per_call: vec![
                ("instructions", 100.0),
                ("instructions", 50.0),
                ("dtlb:a", 10.0),
                ("dtlb:b", 5.0),
            ],
        };
        let mut s = Sampler::new(
            session(&root),
            root.clone(),
            fields(),
            Some(fake),
            Some(Bytes(0)),
            vec![],
        );
        assert!(s.cpu_tick(1.0).is_err(), "not started");
        assert!(s.interval_tick(1.0).is_err(), "not started");
        s.start(1000.0).unwrap();
        let c1 = s.cpu_tick(1001.0).unwrap();
        assert_eq!(c1.scope, "user.slice/user-1000.slice/session-9.scope");
        assert_eq!(c1.cores, 0.0, "fixture files do not move");
        let c2 = s.cpu_tick(1002.0).unwrap();
        assert_eq!(c2.t, 1002.0);
        let sm = s.interval_tick(1010.0).unwrap();
        assert_eq!(sm.dt, 10.0);
        assert_eq!(sm.values["cpu_usage_cores"], 0.0);
        // one read at start, one now: each counter advanced by one step
        assert_eq!(sm.values["instructions_per_sec"], 15.0, "(100+50)/10");
        assert_eq!(sm.values["dtlb_misses_per_sec"], 1.5, "(10+5)/10");
        assert_eq!(
            sm.values["l3_misses_per_sec"], 0.0,
            "unreported counters count as zero"
        );
        assert_eq!(
            sm.values["dram_bytes_per_sec"], 100.0,
            "1000 bytes over 10 s"
        );
        assert_eq!(
            sm.values["memory_current_bytes"], 3_449_266_176.0,
            "mean of two identical reads"
        );
        assert_eq!(
            sm.values["pagecache_bytes"],
            (2_248_900_608u64 - 561_152) as f64
        );
        assert!(
            !sm.values.contains_key("cpu_limit_cores"),
            "fixture has no quota"
        );
        assert!(!sm.values.contains_key("memory_limit_bytes"));

        // next interval starts fresh: with no cpu ticks it reads memory once itself
        let sm2 = s.interval_tick(1020.0).unwrap();
        assert_eq!(sm2.values["anon_bytes"], 933_859_328.0);
        assert_eq!(sm2.values["instructions_per_sec"], 15.0);
        assert_eq!(sm2.dt, 10.0);
    }

    #[test]
    fn interval_without_counters_or_bandwidth() {
        let root = fixture_root();
        let mut s: Sampler<Fake, NoBw> = Sampler::new(
            session(&root),
            root.clone(),
            BTreeMap::new(),
            None,
            None,
            vec![],
        );
        s.start(0.0).unwrap();
        s.cpu_tick(1.0).unwrap();
        let sm = s.interval_tick(60.0).unwrap();
        assert_eq!(
            sm.values.keys().cloned().collect::<Vec<_>>(),
            [
                "anon_bytes",
                "cpu_usage_cores",
                "memory_current_bytes",
                "pagecache_bytes",
                "shmem_bytes"
            ]
        );
        let zero_dt = s.interval_tick(60.0).unwrap();
        assert_eq!(
            zero_dt.values["cpu_usage_cores"], 0.0,
            "no division by zero"
        );
        let same_t = s.cpu_tick(60.0).unwrap();
        assert_eq!(same_t.cores, 0.0);
    }

    #[test]
    fn provenance_covers_every_field_the_sampler_emits() {
        let root = fixture_root();
        let s = Sampler::<Fake, Bytes>::new(
            session(&root),
            root.clone(),
            fields(),
            None,
            Some(Bytes(0)),
            vec![],
        );
        let p = s.provenance();
        assert_eq!(
            p["cpu_usage_cores"],
            Provenance {
                scope: profile::Scope::Cgroup,
                mode: Mode::NotApplicable,
                method: Method::Cgroup
            }
        );
        assert_eq!(p["cpu_usage_cv_1s"].method, Method::Cgroup);
        assert_eq!(p["pagecache_bytes"].method, Method::Cgroup);
        assert_eq!(p["cpu_limit_cores"].method, Method::Cgroup);
        assert_eq!(p["dram_bytes_per_sec"].method, Method::Resctrl);
        assert_eq!(p["l3_misses_per_sec"].method, Method::Proxy);
        assert_eq!(
            p["l3_mpki"].method,
            Method::Proxy,
            "ratios inherit their numerator"
        );
        assert_eq!(p["dtlb_pki"].method, Method::Perf);
        assert_eq!(p["ipc"].method, Method::Perf, "ipc follows instructions");
        assert!(p.keys().all(|k| crate::profile::field(k).is_some()));

        let me = Sampler::<Fake, NoBw>::new(
            Scope::SelfProcess,
            Root::default(),
            BTreeMap::new(),
            None,
            None,
            vec![],
        );
        let p = me.provenance();
        assert_eq!(p["cpu_usage_cores"].method, Method::Rusage);
        assert_eq!(p["pagecache_bytes"].method, Method::Cachestat);
        assert_eq!(p["anon_bytes"].method, Method::Proc);
        assert_eq!(p["anon_bytes"].scope, profile::Scope::Process);
        assert!(!p.contains_key("cpu_limit_cores"));
        assert!(!p.contains_key("dram_bytes_per_sec"));

        let host = Sampler::<Fake, NoBw>::new(
            Scope::Host,
            Root::default(),
            BTreeMap::new(),
            None,
            None,
            vec![],
        );
        assert_eq!(host.provenance()["cpu_usage_cores"].method, Method::Proc);
        assert_eq!(host.scope().kind(), profile::Scope::Host);
    }

    #[test]
    fn self_scope_live_ticks_see_work() {
        let mut s: Sampler<Fake, NoBw> = Sampler::new(
            Scope::SelfProcess,
            Root::default(),
            BTreeMap::new(),
            None,
            None,
            vec![],
        );
        s.start(0.0).unwrap();
        let mut x = 0u64;
        for i in 0..30_000_000u64 {
            x = std::hint::black_box(x.wrapping_add(i ^ (i >> 3)));
        }
        let c = s.cpu_tick(0.05).unwrap();
        assert!(c.cores > 0.0, "{c:?}");
        assert_eq!(c.scope, "self");
        let sm = s.interval_tick(0.1).unwrap();
        assert!(sm.values["cpu_usage_cores"] > 0.0);
        assert!(sm.values["anon_bytes"] > 0.0);
        assert_eq!(
            sm.values["pagecache_bytes"], 0.0,
            "no cache files registered"
        );
    }

    #[test]
    fn live_counters_need_a_cgroup_handle_for_cgroup_targets() {
        use crate::perf::Spec;
        use crate::plan::{Group, Plan};
        let plan = Plan {
            groups: vec![Group {
                target: TargetPlan::CgroupOnCpu(0),
                specs: vec![Spec::instructions()],
                opts: Options::default(),
            }],
            fields: BTreeMap::new(),
            notes: vec![],
        };
        let err = LiveCounters::open(&plan, None).unwrap_err();
        assert_eq!(err.name, "instructions");
        assert!(err.source.to_string().contains("no cgroup directory"));
        let empty = LiveCounters::open(&Plan::default(), None).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);

        let gone = Plan {
            groups: vec![Group {
                target: TargetPlan::Task(i32::MAX - 1),
                specs: vec![Spec::instructions()],
                opts: Options::default(),
            }],
            fields: BTreeMap::new(),
            notes: vec![],
        };
        let strict = LiveCounters::open(&gone, None).unwrap_err();
        assert_eq!(strict.source.raw_os_error(), Some(libc::ESRCH), "{strict}");
        let (live, notes) = LiveCounters::open_with(&gone, None, true).unwrap();
        assert!(live.is_empty());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("exited before"));
    }
}

#[cfg(all(test, feature = "hw-tests"))]
mod hw_tests {
    use super::*;
    use crate::cpu;
    use crate::events::{Sysfs, table_for};
    use crate::plan::{Access, ScopeInfo, plan};
    use crate::topology::Topology;

    struct NoBw;
    impl BandwidthSource for NoBw {
        fn read_bytes(&mut self) -> io::Result<Option<u64>> {
            unreachable!()
        }
    }

    #[test]
    fn planned_counters_on_self_produce_rates() {
        let ident = cpu::detect().unwrap();
        let table = table_for(&ident);
        let sysfs = Sysfs::default();
        let topo = Topology::read(&sysfs).unwrap();
        let access = Access {
            self_user: true,
            self_kernel: false,
            per_cpu: false,
            attach_same_uid: true,
        };
        let p = plan(
            &ScopeInfo::SelfThreads { pinned_cpus: None },
            &access,
            table,
            &sysfs,
            &topo,
        )
        .unwrap();
        let mut live = LiveCounters::open(&p, None).unwrap();
        live.enable().unwrap();
        let mut s: Sampler<LiveCounters, NoBw> = Sampler::new(
            Scope::SelfProcess,
            Root::default(),
            p.fields.clone(),
            Some(live),
            None,
            vec![],
        );
        s.start(0.0).unwrap();
        let v: Vec<u64> = (0..8_000_000).collect();
        std::hint::black_box(v.iter().sum::<u64>());
        let sm = s.interval_tick(1.0).unwrap();
        assert!(sm.values["instructions_per_sec"] > 1e6, "{:?}", sm.values);
        assert!(sm.values["cycles_per_sec"] > 1e6);
        assert!(sm.values.contains_key("dtlb_misses_per_sec"));
        assert!(sm.values.contains_key("itlb_misses_per_sec"));
        assert!(sm.values.contains_key("l3_misses_per_sec"));
    }
}
