//! The `run` command: allocate the footprint, open counters, start the
//! workers, then fit knobs window by window and report.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::sched::sched_getaffinity;
use nix::unistd::Pid;

use crate::aggregate::pooled_cv;
use crate::caps::{Expectations, Facts, Root};
use crate::cpu;
use crate::events::{Sysfs, Vendor, table_for};
use crate::fit::{Fitter, Goal, Limits, Measured, Status, TlbRegime};
use crate::footprint::{AnonBuffer, CacheFile, KernelFiles, ShmemBuffer, is_tmpfs};
use crate::loadgen::{Buffers, Generator, Knobs, PAGE};
use crate::measure::{self, Scope};
use crate::perf;
use crate::plan::{Access, ScopeInfo, plan};
use crate::profile::{self, Method, Mode, Provenance, Targets};
use crate::record::{stop_requested, unix_now};
use crate::report::{MemoryRow, Report, TargetRow, Tier, WindowRow, WindowValue};
use crate::resctrl::{MonGroup, Resctrl};
use crate::sampler::{LiveCounters, Sampler};
use crate::samples::Sample;
use crate::topology::Topology;

/// Whether to pin workers to CPUs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pin {
    /// Pin when it buys per-core L3 attribution: AMD with CAP_PERFMON.
    Auto,
    /// Always.
    Yes,
    /// Never.
    No,
}

/// How a run behaves.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Stop after this long; `None` runs until interrupted.
    pub duration_s: Option<f64>,
    /// Measurement window; `None` sizes it from the burstiness target.
    pub window_s: Option<f64>,
    /// Disk-backed directory for the page-cache file and slab files.
    pub scratch_dir: PathBuf,
    /// Pinning policy.
    pub pin: Pin,
    /// Seed for the streams and the burstiness draw.
    pub seed: u64,
    /// Keep adjusting after convergence.
    pub adaptive: bool,
    /// Fitter passes before holding regardless.
    pub max_passes: u32,
    /// Worker threads; `None` derives them from the targets.
    pub threads: Option<usize>,
    /// Main buffer size; `None` derives it from the anonymous target.
    pub main_bytes: Option<u64>,
    /// Pages reserved for the sparse stream.
    pub sparse_capacity: usize,
    /// Executable pages reserved for the code stream.
    pub code_capacity: usize,
    /// Ask for transparent hugepages on the main buffer.
    pub huge: bool,
    /// Fixed knobs: no fitting, just run and report.
    pub knobs: Option<Knobs>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            duration_s: None,
            window_s: None,
            scratch_dir: PathBuf::from("."),
            pin: Pin::Auto,
            seed: 1,
            adaptive: false,
            max_passes: 12,
            threads: None,
            main_bytes: None,
            sparse_capacity: 16384,
            code_capacity: 16384,
            huge: true,
            knobs: None,
        }
    }
}

/// Default main buffer when no anonymous target is given: far beyond
/// any L3.
pub const DEFAULT_MAIN_BYTES: u64 = 1 << 30;
/// Smallest main buffer the streams make sense on.
pub const MIN_MAIN_BYTES: u64 = 1 << 20;

/// Window length: the burstiness tolerance needs `(cv / tol)^2`
/// seconds of 1 Hz samples, between 30 s and 300 s; an explicit
/// request overrides.
pub fn window_seconds(goal_cv: Option<f64>, tol: f64, requested: Option<f64>) -> f64 {
    if let Some(r) = requested {
        return r.max(0.1);
    }
    let need = goal_cv.map(|cv| (cv / tol).powi(2)).unwrap_or(0.0);
    need.clamp(30.0, 300.0)
}

/// Sizes of the footprint pieces for a target set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryPlan {
    /// Main buffer.
    pub main_bytes: u64,
    /// Page-cache file, if asked for.
    pub pagecache_bytes: u64,
    /// Shared memory, if asked for.
    pub shmem_bytes: u64,
    /// Empty files for kernel memory, if asked for.
    pub kernel_files: usize,
}

/// Size the footprint: the main buffer is the anonymous target less
/// what the process already holds and what the other streams' pages
/// add; kernel memory is whatever `memory_current_bytes` exceeds the
/// three components by.
pub fn plan_memory(
    t: &Targets,
    baseline_anon: u64,
    sparse_pages: usize,
    code_pages: usize,
    main_override: Option<u64>,
) -> MemoryPlan {
    let stream_pages = ((sparse_pages + code_pages) * PAGE) as u64;
    let main_bytes = match (main_override, t.get("anon_bytes")) {
        (Some(m), _) => m,
        (None, Some(anon)) => (anon as u64)
            .saturating_sub(baseline_anon)
            .saturating_sub(stream_pages),
        (None, None) => DEFAULT_MAIN_BYTES,
    }
    .max(MIN_MAIN_BYTES);
    let pagecache_bytes = t.get("pagecache_bytes").unwrap_or(0.0).max(0.0) as u64;
    let shmem_bytes = t.get("shmem_bytes").unwrap_or(0.0).max(0.0) as u64;
    let kernel_files = match t.get("memory_current_bytes") {
        Some(total) => {
            let parts = t.get("anon_bytes").unwrap_or(0.0)
                + t.get("pagecache_bytes").unwrap_or(0.0)
                + t.get("shmem_bytes").unwrap_or(0.0);
            KernelFiles::files_for((total - parts).max(0.0) as u64)
        }
        None => 0,
    };
    MemoryPlan {
        main_bytes,
        pagecache_bytes,
        shmem_bytes,
        kernel_files,
    }
}

/// DRAM traffic the generator knows it causes: reads are the counted
/// L3 misses, writes are the non-temporal lines plus the dirty lines
/// of the main stream written back, 64 bytes each.
pub fn dram_model(sample: &Sample, knobs: &Knobs, accesses_per_s: f64) -> Option<f64> {
    let misses = sample.values.get("l3_misses_per_sec")?;
    Some(64.0 * (misses + accesses_per_s * (f64::from(knobs.ntlines) + knobs.wfrac * knobs.lines)))
}

/// Profile units from one window's sample.
pub fn measured_from(
    sample: &Sample,
    cv: Option<f64>,
    accesses_per_s: f64,
    dram: Option<f64>,
) -> Measured {
    let v = |k: &str| sample.values.get(k).copied();
    let instr = v("instructions_per_sec").filter(|i| *i > 0.0);
    let ratio = |num: Option<f64>, den: Option<f64>, scale: f64| Some(num? / den? * scale);
    Measured {
        cores: v("cpu_usage_cores"),
        cv,
        ipc: ratio(instr, v("cycles_per_sec").filter(|c| *c > 0.0), 1.0),
        mpki: ratio(v("l3_misses_per_sec"), instr, 1000.0),
        dram,
        dtlb_pki: ratio(v("dtlb_misses_per_sec"), instr, 1000.0),
        itlb_pki: ratio(v("itlb_misses_per_sec"), instr, 1000.0),
        accesses_per_s,
    }
}

/// CPUs this process may run on, ascending.
pub fn affinity_cpus() -> Result<Vec<u32>> {
    let set = sched_getaffinity(Pid::from_raw(0)).context("sched_getaffinity")?;
    let cpus: Vec<u32> = (0..libc::CPU_SETSIZE as usize)
        .filter(|i| set.is_set(*i).unwrap_or(false))
        .map(|i| i as u32)
        .collect();
    if cpus.is_empty() {
        bail!("affinity mask is empty");
    }
    Ok(cpus)
}

/// Pick `n` CPUs, one per physical core first, so SMT siblings only
/// pair up once every core is in use.
pub fn choose_cpus(affinity: &[u32], topo: &Topology, n: usize) -> Vec<u32> {
    let mut first: Vec<u32> = Vec::new();
    let mut rest: Vec<u32> = Vec::new();
    for &c in affinity {
        match topo.cpu(c) {
            Some(cpu) if cpu.siblings.iter().find(|s| affinity.contains(s)) == Some(&c) => {
                first.push(c)
            }
            _ => rest.push(c),
        }
    }
    first.extend(rest);
    first.into_iter().take(n).collect()
}

/// The memory the sampler should see as the process's page cache.
fn cache_handles(cache: &Option<CacheFile>) -> Vec<File> {
    cache
        .iter()
        .filter_map(|c| c.file().try_clone().ok())
        .collect()
}

struct Window {
    cpu_series: Vec<f64>,
    accesses_at_start: u64,
    started: Instant,
}

/// Run the generator against `targets`, writing progress lines to
/// `progress`, and return the report.
pub fn run(targets: &Targets, settings: &Settings, progress: &mut dyn Write) -> Result<Report> {
    if targets.is_empty() {
        bail!("no targets");
    }
    let root = Root::default();
    let facts = Facts::detect(&root);
    let rules = Expectations::from_facts(&facts);
    let access = Access::new(&rules, &perf::probe());
    let ident = cpu::detect();
    let table = ident.as_ref().and_then(table_for);
    let vendor = ident.as_ref().and_then(Vendor::of);
    let sysfs = Sysfs::default();
    let topo = Topology::read(&sysfs).context("read CPU topology")?;
    let affinity = affinity_cpus()?;
    let mut notes: Vec<String> = Vec::new();

    let threads = settings
        .threads
        .unwrap_or_else(|| Fitter::threads_for(targets, affinity.len()))
        .clamp(1, affinity.len());
    let pin = match settings.pin {
        Pin::Yes => true,
        Pin::No => false,
        Pin::Auto => access.per_cpu && vendor == Some(Vendor::Amd),
    };
    let pinned: Option<Vec<u32>> = pin.then(|| choose_cpus(&affinity, &topo, threads));

    // Footprint first, so its page faults do not land in a window.
    let baseline = measure::memory(&Scope::SelfProcess, &root, &[]).context("baseline memory")?;
    let mem_plan = plan_memory(
        targets,
        baseline.anon.unwrap_or(0),
        settings.sparse_capacity,
        settings.code_capacity,
        settings.main_bytes,
    );
    if mem_plan.pagecache_bytes > 0 && is_tmpfs(&settings.scratch_dir).unwrap_or(false) {
        notes.push(format!(
            "{} is tmpfs: the page-cache file there counts as shmem, not page cache",
            settings.scratch_dir.display()
        ));
    }
    let buffers = Arc::new(
        Buffers::new(
            mem_plan.main_bytes as usize,
            settings.huge,
            settings.sparse_capacity,
            settings.code_capacity,
        )
        .context("allocate the main, sparse and code buffers")?,
    );
    let mut cache = if mem_plan.pagecache_bytes > 0 {
        Some(
            CacheFile::create(&settings.scratch_dir, mem_plan.pagecache_bytes)
                .context("create the page-cache file")?,
        )
    } else {
        None
    };
    let _shmem = if mem_plan.shmem_bytes > 0 {
        Some(ShmemBuffer::new(mem_plan.shmem_bytes as usize).context("create shared memory")?)
    } else {
        None
    };
    let mut _kernel = None;
    if mem_plan.kernel_files > 0 {
        let mut k = KernelFiles::new(&settings.scratch_dir).context("create the slab directory")?;
        k.set_count(mem_plan.kernel_files)
            .context("create slab files")?;
        notes.push(format!(
            "kernel memory: {} empty files, open-loop at {} bytes each",
            mem_plan.kernel_files,
            crate::footprint::BYTES_PER_EMPTY_FILE
        ));
        _kernel = Some(k);
    }
    let mut extra_anon: Vec<AnonBuffer> = Vec::new();

    // Bandwidth: a resctrl group with this thread in it, inherited by the workers.
    let bandwidth: Option<MonGroup> = match Resctrl::from_facts(&root, &facts) {
        Some(rc) => match rc
            .create_mon_group(&format!("thrashgen-{}", std::process::id()))
            .and_then(|g| g.add_self().map(|()| g))
        {
            Ok(g) => Some(g),
            Err(e) => {
                notes.push(format!(
                    "DRAM bandwidth modelled: cannot use a resctrl group: {e}"
                ));
                None
            }
        },
        None => {
            notes.push(
                "DRAM bandwidth modelled: resctrl is not mounted with mbm_total_bytes".into(),
            );
            None
        }
    };

    // Counters, before the workers exist so inherit covers them.
    let plan = plan(
        &ScopeInfo::SelfThreads {
            pinned_cpus: pinned.clone(),
        },
        &access,
        table,
        &sysfs,
        &topo,
    )
    .context("plan counters")?;
    notes.extend(plan.notes.iter().cloned());
    let (mut live, skipped) =
        LiveCounters::open_with(&plan, None, false).context("open counters")?;
    notes.extend(skipped);
    live.enable().context("enable counters")?;
    let counters = if live.is_empty() { None } else { Some(live) };
    let mut sampler: Sampler<LiveCounters, MonGroup> = Sampler::new(
        Scope::SelfProcess,
        root.clone(),
        plan.fields.clone(),
        counters,
        bandwidth,
        cache_handles(&cache),
    );
    let mut provenance = sampler.provenance();
    if !provenance.contains_key("dram_bytes_per_sec")
        && provenance.contains_key("l3_misses_per_sec")
    {
        provenance.insert(
            "dram_bytes_per_sec".into(),
            Provenance {
                scope: profile::Scope::Process,
                mode: Mode::NotApplicable,
                method: Method::Model,
            },
        );
    }

    // Fitter and first knobs.
    let goal = Goal::from_targets(targets);
    let limits = Limits {
        sparse_capacity: buffers.sparse_capacity(),
        code_capacity: buffers.code_capacity(),
    };
    let mut fitter = Fitter::new(threads, limits);
    let regime = if vendor == Some(Vendor::Amd) {
        TlbRegime::L2Hit
    } else {
        TlbRegime::Walk
    };
    let mut knobs = settings
        .knobs
        .clone()
        .unwrap_or_else(|| fitter.initial_knobs(&goal, regime));
    let fitting = settings.knobs.is_none() && !goal.is_empty();
    let window_s = window_seconds(goal.cv, fitter.tol.cv, settings.window_s);
    writeln!(
        progress,
        "run: {threads} threads, window {window_s:.0} s, main buffer {} MiB{}",
        mem_plan.main_bytes >> 20,
        if fitting { "" } else { ", fixed knobs" }
    )?;

    let start = Instant::now();
    sampler.start(unix_now()).context("initial readings")?;
    let generator = Generator::start(
        Arc::clone(&buffers),
        knobs.clone(),
        threads,
        pinned.as_deref(),
        settings.seed,
    )?;

    let accesses_total = |g: &Generator| {
        g.stats()
            .iter()
            .map(|s| s.accesses.load(Ordering::Relaxed))
            .sum::<u64>()
    };
    let mut window = Window {
        cpu_series: Vec::new(),
        accesses_at_start: 0,
        started: Instant::now(),
    };
    let mut windows: Vec<WindowRow> = Vec::new();
    let mut last_statuses: Vec<Status> = Vec::new();
    let mut last_sample: Option<Sample> = None;
    let mut converged = false;
    let cpu_step = Duration::from_secs(1);
    let window_len = Duration::from_secs_f64(window_s);
    let deadline = settings
        .duration_s
        .map(|d| start + Duration::from_secs_f64(d));
    let mut next_cpu = start + cpu_step;
    let mut window_end = window.started + window_len;

    loop {
        let stop = stop_requested() || deadline.is_some_and(|d| Instant::now() >= d);
        if !stop {
            let mut wake = next_cpu.min(window_end);
            if let Some(d) = deadline {
                wake = wake.min(d);
            }
            while Instant::now() < wake && !stop_requested() {
                std::thread::sleep(
                    wake.saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(50)),
                );
            }
        }
        let now = Instant::now();
        if now >= next_cpu || stop {
            let c = sampler.cpu_tick(unix_now()).context("busy-cores sample")?;
            window.cpu_series.push(c.cores);
            next_cpu += cpu_step;
        }
        if now >= window_end || stop {
            let seconds = window.started.elapsed().as_secs_f64();
            if seconds >= window_s * 0.5 || windows.is_empty() {
                let sample = sampler.interval_tick(unix_now()).context("window sample")?;
                let accesses = accesses_total(&generator);
                let aps = (accesses - window.accesses_at_start) as f64 / seconds.max(1e-9);
                let cv = pooled_cv(
                    std::slice::from_ref(&window.cpu_series),
                    window.cpu_series.len().max(1),
                )
                .map(|(cv, _)| cv);
                let dram = sample
                    .values
                    .get("dram_bytes_per_sec")
                    .copied()
                    .or_else(|| dram_model(&sample, &knobs, aps));
                let m = measured_from(&sample, cv, aps, dram);
                let statuses = fitter.statuses(&m, &goal);
                writeln!(
                    progress,
                    "window {:>4.0}s pass {:>2}: {}",
                    start.elapsed().as_secs_f64(),
                    fitter.passes,
                    statuses
                        .iter()
                        .map(|s| format!(
                            "{}={}{}",
                            s.key,
                            s.measured
                                .map(|v| format!("{v:.4}"))
                                .unwrap_or_else(|| "-".into()),
                            s.error.map(|e| format!("({e:+.1}%)")).unwrap_or_default()
                        ))
                        .collect::<Vec<_>>()
                        .join(" ")
                )?;
                windows.push(WindowRow {
                    end_s: start.elapsed().as_secs_f64(),
                    seconds,
                    pass: fitter.passes,
                    knobs: knobs.clone(),
                    values: statuses
                        .iter()
                        .map(|s| WindowValue {
                            key: s.key.into(),
                            measured: s.measured,
                            error: s.error,
                        })
                        .collect(),
                });
                converged = Fitter::converged(&statuses);
                last_statuses = statuses;
                // Memory: top up anonymous memory if short; restore page cache.
                if let (Some(target), Some(now_anon)) =
                    (targets.get("anon_bytes"), sample.values.get("anon_bytes"))
                {
                    if *now_anon < target * 0.99 {
                        if let Ok(b) = AnonBuffer::new((target - now_anon) as usize, settings.huge)
                        {
                            extra_anon.push(b);
                        }
                    } else if *now_anon > target * 1.05
                        && extra_anon.is_empty()
                        && !notes.iter().any(|n| n.starts_with("anonymous memory"))
                    {
                        notes.push(format!(
                            "anonymous memory is {:.0} bytes above the target; the main buffer cannot shrink while workers use it",
                            now_anon - target
                        ));
                    }
                }
                if let Some(c) = cache.as_mut() {
                    if c.resident_bytes()
                        .map(|r| r < c.len() * 95 / 100)
                        .unwrap_or(false)
                    {
                        let _ = c.reread();
                    }
                }
                last_sample = Some(sample);
                if fitting && !stop && (settings.adaptive || !converged) {
                    if fitter.passes < settings.max_passes {
                        let next = fitter.step(&knobs, &m, &goal);
                        if next != knobs {
                            generator.set_knobs(next.clone(), &buffers)?;
                            knobs = next;
                        }
                    } else if !notes.iter().any(|n| n.starts_with("not within tolerance")) {
                        notes.push(format!(
                            "not within tolerance after {} passes; holding",
                            fitter.passes
                        ));
                    }
                }
            }
            window = Window {
                cpu_series: Vec::new(),
                accesses_at_start: accesses_total(&generator),
                started: Instant::now(),
            };
            window_end = window.started + window_len;
        }
        if stop {
            break;
        }
    }
    generator.stop();
    // Read at the end: khugepaged may have collapsed pages since the start.
    let thp_bytes = buffers.main().huge_bytes().ok();

    let last_values = last_sample.map(|s| s.values).unwrap_or_default();
    let target_rows: Vec<TargetRow> = targets
        .targets()
        .filter(|(f, _)| {
            !matches!(
                f.key,
                "memory_current_bytes" | "anon_bytes" | "pagecache_bytes" | "shmem_bytes"
            )
        })
        .map(|(f, t)| {
            let st = last_statuses.iter().find(|s| s.key == f.key);
            let prov = provenance.get(f.key).copied();
            let measured = st.and_then(|s| s.measured);
            TargetRow {
                key: f.key.into(),
                target: t,
                tier: if measured.is_some() {
                    Tier::of(prov.as_ref())
                } else {
                    Tier::Unverified
                },
                provenance: prov,
                measured,
                error: st.and_then(|s| s.error),
                within: st.is_some_and(|s| s.within),
            }
        })
        .collect();
    let memory_rows: Vec<MemoryRow> = [
        "memory_current_bytes",
        "anon_bytes",
        "pagecache_bytes",
        "shmem_bytes",
    ]
    .iter()
    .filter(|k| targets.get(k).is_some() || last_values.contains_key(**k))
    .map(|k| MemoryRow {
        key: (*k).into(),
        target: targets.get(k),
        measured: last_values.get(*k).copied(),
    })
    .collect();
    Ok(Report {
        cpu: ident,
        table: table.map(|t| t.table.to_string()),
        threads,
        pinned_cpus: generator_pins(pinned, threads),
        knobs,
        main_bytes: mem_plan.main_bytes,
        thp_bytes,
        passes: fitter.passes,
        converged,
        targets: target_rows,
        memory: memory_rows,
        windows,
        notes,
    })
}

fn generator_pins(pinned: Option<Vec<u32>>, threads: usize) -> Option<Vec<u32>> {
    pinned.map(|c| (0..threads).map(|i| c[i % c.len()]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn window_length_follows_burstiness() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(window_seconds(None, 0.05, None), 30.0));
        assert!(close(window_seconds(Some(0.3), 0.05, None), 36.0));
        assert!(close(window_seconds(Some(0.73), 0.05, None), 213.16));
        assert!(close(window_seconds(Some(5.0), 0.05, None), 300.0));
        assert!(close(window_seconds(Some(5.0), 0.05, Some(2.0)), 2.0));
        assert!(close(window_seconds(None, 0.05, Some(0.0)), 0.1));
    }

    #[test]
    fn memory_planning() {
        let mut t = Targets::new();
        let p = plan_memory(&t, 10 << 20, 100, 100, None);
        assert_eq!(
            p,
            MemoryPlan {
                main_bytes: DEFAULT_MAIN_BYTES,
                pagecache_bytes: 0,
                shmem_bytes: 0,
                kernel_files: 0
            }
        );
        t.set("anon_bytes", 100.0 * (1 << 20) as f64).unwrap();
        let p = plan_memory(&t, 10 << 20, 1024, 1024, None);
        assert_eq!(p.main_bytes, (100 << 20) - (10 << 20) - 2048 * PAGE as u64);
        t.set("anon_bytes", 1.0).unwrap();
        assert_eq!(
            plan_memory(&t, 10 << 20, 1, 1, None).main_bytes,
            MIN_MAIN_BYTES,
            "floored"
        );
        assert_eq!(
            plan_memory(&t, 0, 1, 1, Some(5 << 20)).main_bytes,
            5 << 20,
            "override wins"
        );
        t.set("pagecache_bytes", 3.0e6).unwrap();
        t.set("shmem_bytes", 2.0e6).unwrap();
        t.set("memory_current_bytes", 5.0e6 + 1.0 + 1_100_000.0)
            .unwrap();
        let p = plan_memory(&t, 0, 1, 1, None);
        assert_eq!(
            (p.pagecache_bytes, p.shmem_bytes, p.kernel_files),
            (3_000_000, 2_000_000, 1000)
        );
        t.set("memory_current_bytes", 1.0).unwrap();
        assert_eq!(
            plan_memory(&t, 0, 1, 1, None).kernel_files,
            0,
            "no negative kernel memory"
        );
    }

    #[test]
    fn measured_and_model() {
        let mut values = BTreeMap::new();
        values.insert("cpu_usage_cores".to_string(), 2.0);
        values.insert("instructions_per_sec".to_string(), 1.0e9);
        values.insert("cycles_per_sec".to_string(), 5.0e8);
        values.insert("l3_misses_per_sec".to_string(), 2.0e6);
        values.insert("dtlb_misses_per_sec".to_string(), 1.0e6);
        let sample = Sample {
            t: 0.0,
            scope: "self".into(),
            dt: 1.0,
            values,
        };
        let knobs = Knobs {
            ntlines: 2,
            wfrac: 0.5,
            lines: 4.0,
            ..Knobs::default()
        };
        let model = dram_model(&sample, &knobs, 1.0e5).unwrap();
        assert_eq!(model, 64.0 * (2.0e6 + 1.0e5 * (2.0 + 2.0)));
        let m = measured_from(&sample, Some(0.2), 1.0e5, Some(model));
        assert_eq!(m.cores, Some(2.0));
        assert_eq!(m.cv, Some(0.2));
        assert_eq!(m.ipc, Some(2.0));
        assert_eq!(m.mpki, Some(2.0));
        assert_eq!(m.dtlb_pki, Some(1.0));
        assert_eq!(m.itlb_pki, None);
        assert_eq!(m.dram, Some(model));
        assert_eq!(m.accesses_per_s, 1.0e5);
        let empty = Sample {
            t: 0.0,
            scope: "self".into(),
            dt: 1.0,
            values: BTreeMap::new(),
        };
        assert_eq!(dram_model(&empty, &knobs, 1.0), None);
        let m = measured_from(&empty, None, 0.0, None);
        assert_eq!(m, Measured::default());
    }

    #[test]
    fn cpu_choice_spreads_over_cores() {
        let topo = Topology::read(&Sysfs::at(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/root"
        )))
        .unwrap();
        let affinity = [0, 1, 8, 32, 33];
        assert_eq!(
            choose_cpus(&affinity, &topo, 3),
            [0, 1, 8],
            "one per core before siblings"
        );
        assert_eq!(choose_cpus(&affinity, &topo, 5), [0, 1, 8, 32, 33]);
        assert_eq!(choose_cpus(&[33, 1], &topo, 2), [1, 33]);
        assert_eq!(
            choose_cpus(&[99], &topo, 1),
            [99],
            "unknown CPUs still usable"
        );
        assert_eq!(generator_pins(Some(vec![4, 5]), 3), Some(vec![4, 5, 4]));
        assert_eq!(generator_pins(None, 3), None);
        assert!(!affinity_cpus().unwrap().is_empty());
    }

    #[test]
    fn run_needs_targets_and_a_scratch_dir() {
        let mut out = Vec::new();
        assert!(
            run(&Targets::new(), &Settings::default(), &mut out)
                .unwrap_err()
                .to_string()
                .contains("no targets")
        );
        let mut t = Targets::new();
        t.set("pagecache_bytes", 1.0e6).unwrap();
        let settings = Settings {
            scratch_dir: "/nonexistent".into(),
            main_bytes: Some(1 << 20),
            duration_s: Some(0.1),
            ..Settings::default()
        };
        assert!(
            run(&t, &settings, &mut out)
                .unwrap_err()
                .to_string()
                .contains("page-cache file")
        );
    }
}
