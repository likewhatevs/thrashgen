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
use crate::loadgen::{Buffers, Generator, Knobs, PAGE, SparseSet};
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
    /// Open counters on the workers. Off, the run takes no PMU
    /// counters, so a recorder watching it has them to itself, and
    /// every counter target is reported unverified.
    pub counters: bool,
    /// Knobs to start fitting from, typically a calibration.
    pub initial_knobs: Option<Knobs>,
    /// Stop once two consecutive windows are within tolerance.
    pub stop_when_converged: bool,
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
            max_passes: 36,
            threads: None,
            main_bytes: None,
            sparse_capacity: 16384,
            code_capacity: 16384,
            huge: true,
            knobs: None,
            counters: true,
            initial_knobs: None,
            stop_when_converged: false,
        }
    }
}

/// Consecutive windows within tolerance before convergence is declared.
pub const CONVERGED_WINDOWS: u32 = 2;
/// How long a converged fit must be out of tolerance before it moves
/// again. Two 100 ms windows were not enough: a hiccup of that length,
/// a page migration or a neighbour's burst, sent ops fourfold.
pub const REFIT_SECONDS: f64 = 1.0;

/// Default main buffer when no anonymous target is given: far beyond
/// any L3.
pub const DEFAULT_MAIN_BYTES: u64 = 1 << 30;
/// Smallest main buffer the streams make sense on.
pub const MIN_MAIN_BYTES: u64 = 1 << 20;

/// Fit window length. The workers take new knobs within a 10 ms
/// period and the ratios are read over the whole window; measured at
/// fixed knobs on Zen 4, IPC and MPKI vary 0.3 to 0.5 percent from one
/// 100 ms window to the next and the TLB rates about one percent, far
/// inside the five percent tolerance, so 100 ms costs nothing
/// measurable and thirty-six passes fit in under four seconds. An
/// explicit request overrides.
pub fn window_seconds(requested: Option<f64>) -> f64 {
    requested.map(|r| r.max(0.05)).unwrap_or(0.1)
}

/// Long-run busy cores and burstiness predicted from the seconds
/// measured so far. Each second's busy cores are compared with what the
/// knobs intended for it, so the mean's efficiency and the burstiness
/// ratio are known from a few seconds where a long-run average would
/// take as many seconds as the CV squared over the tolerance squared;
/// applied to the clipped model's mean and CV they give the values a
/// long recording would see. With no intended variation the measured
/// jitter is the burstiness.
pub fn predicted_cores_cv(
    measured: &[f64],
    intended: &[f64],
    knobs: &Knobs,
    threads: usize,
) -> (Option<f64>, Option<f64>) {
    if measured.is_empty() {
        return (None, None);
    }
    let (model_mean, model_cv) = crate::loadgen::clipped_duty_stats(knobs.duty, knobs.cv);
    let sum_int: f64 = intended.iter().sum();
    let efficiency = if sum_int > 0.0 {
        measured.iter().sum::<f64>() / sum_int
    } else {
        1.0
    };
    let cores = efficiency * threads as f64 * model_mean;
    let one =
        |xs: &[f64]| pooled_cv(std::slice::from_ref(&xs.to_vec()), xs.len()).map(|(cv, _)| cv);
    let cv = match (one(measured), one(intended)) {
        (Some(m), Some(i)) if i > 1e-9 => Some(model_cv * m / i),
        (m, _) => m,
    };
    (Some(cores), cv)
}

/// Two consecutive windows at the same knobs are one longer window:
/// their ratios are averaged for the fitter, so a single noisy window,
/// a neighbour's burst or a page-cache refill, moves nothing on its
/// own. Busy cores and burstiness come from the rolling window already.
pub fn blend(now: &Measured, prev: &Measured) -> Measured {
    let avg = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(x), Some(y)) => Some(0.5 * (x + y)),
        (x, _) => x,
    };
    Measured {
        ipc: avg(now.ipc, prev.ipc),
        mpki: avg(now.mpki, prev.mpki),
        dram: avg(now.dram, prev.dram),
        dtlb_pki: avg(now.dtlb_pki, prev.dtlb_pki),
        itlb_pki: avg(now.itlb_pki, prev.itlb_pki),
        instructions_per_s: avg(now.instructions_per_s, prev.instructions_per_s),
        accesses_per_s: 0.5 * (now.accesses_per_s + prev.accesses_per_s),
        ..now.clone()
    }
}

/// Rolling window of one-second samples behind busy cores and
/// burstiness: two factor blocks, so the ratio sees every quantile
/// twice and a neighbour's arrival is in within a minute.
pub const CORES_WINDOW_SECONDS: usize = 2 * crate::loadgen::FACTOR_BLOCK as usize;

/// Whether the sparse stream should run on the page-cache file: the
/// anonymous target cannot spare the pages the configured sparse set
/// needs, and the file has them. Pages visited there count as page
/// cache, which the profile asked for anyway.
pub fn sparse_from_file(anon_cap: usize, configured: usize, pagecache_bytes: u64) -> bool {
    anon_cap < configured && pagecache_bytes >= (configured * PAGE) as u64
}

/// Pages to reserve for the sparse and code streams: the configured
/// capacities, each capped at an eighth of the anonymous target so a
/// small footprint is not swamped by the streams' own pages.
pub fn stream_capacities(anon_target: Option<f64>, sparse: usize, code: usize) -> (usize, usize) {
    match anon_target {
        Some(anon) if anon > 0.0 => {
            let cap = ((anon / 8.0) as usize / PAGE).max(64);
            (sparse.min(cap), code.min(cap))
        }
        _ => (sparse, code),
    }
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
    Some(64.0 * (misses + accesses_per_s * (knobs.ntlines + knobs.wfrac * knobs.lines)))
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
        cv_samples: 0,
        instructions_per_s: instr,
    }
}

/// A note when the memory targets exceed the memory limit the profile
/// carries. The limit is a hint, so the run goes ahead; the cgroup it
/// runs in decides what happens.
pub fn memory_limit_note(t: &Targets) -> Option<String> {
    let limit = t.get("memory_limit_bytes").filter(|l| *l > 0.0)?;
    let total = t.get("memory_current_bytes").unwrap_or_else(|| {
        ["anon_bytes", "pagecache_bytes", "shmem_bytes"]
            .iter()
            .filter_map(|k| t.get(k))
            .sum()
    });
    (total > limit).then(|| {
        format!(
            "memory targets total {total:.0} bytes, above the profile's {limit:.0}-byte memory limit; the limit is not enforced here"
        )
    })
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
    notes.extend(memory_limit_note(targets));

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
    let (sparse_capacity, code_capacity) = stream_capacities(
        targets.get("anon_bytes"),
        settings.sparse_capacity,
        settings.code_capacity,
    );
    let mem_plan = plan_memory(
        targets,
        baseline.anon.unwrap_or(0),
        sparse_capacity,
        code_capacity,
        settings.main_bytes,
    );
    if mem_plan.pagecache_bytes > 0 && is_tmpfs(&settings.scratch_dir).unwrap_or(false) {
        notes.push(format!(
            "{} is tmpfs: the page-cache file there counts as shmem, not page cache",
            settings.scratch_dir.display()
        ));
    }
    let mut cache = if mem_plan.pagecache_bytes > 0 {
        Some(
            CacheFile::create(&settings.scratch_dir, mem_plan.pagecache_bytes)
                .context("create the page-cache file")?,
        )
    } else {
        None
    };
    let on_file = cache.as_ref().filter(|_| {
        sparse_from_file(
            sparse_capacity,
            settings.sparse_capacity,
            mem_plan.pagecache_bytes,
        )
    });
    let sparse = match on_file {
        Some(c) => {
            notes.push(format!(
                "sparse stream on the page-cache file: {} pages, which the anonymous footprint could not spare",
                settings.sparse_capacity
            ));
            SparseSet::File(
                c.map(settings.sparse_capacity * PAGE)
                    .context("map the page-cache file for the sparse stream")?,
            )
        }
        None => SparseSet::Anon(
            crate::footprint::AnonBuffer::new(sparse_capacity.max(1) * PAGE, false)
                .context("allocate the sparse buffer")?,
        ),
    };
    let buffers = Arc::new(
        Buffers::with_sparse(
            mem_plan.main_bytes as usize,
            settings.huge,
            sparse,
            code_capacity,
        )
        .context("allocate the main, sparse and code buffers")?,
    );
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
            .acquire_group(&format!("thrashgen-{}", std::process::id()))
            .and_then(|g| g.add_self().map(|()| g))
        {
            Ok(g) => {
                if !g.is_owned() {
                    notes.push(format!(
                        "DRAM bandwidth from the existing resctrl group {}",
                        g.dir().display()
                    ));
                }
                Some(g)
            }
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
    let plan = if settings.counters {
        plan(
            &ScopeInfo::SelfThreads {
                pinned_cpus: pinned.clone(),
            },
            &access,
            table,
            &sysfs,
            &topo,
        )
        .context("plan counters")?
    } else {
        notes.push("counters not opened: counter targets are unverified".into());
        crate::plan::Plan::default()
    };
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
    let regime = if vendor == Some(Vendor::Amd) {
        TlbRegime::L2Hit
    } else {
        TlbRegime::Walk
    };
    let limits = Limits {
        sparse_capacity: buffers.sparse_capacity(),
        sparse_floor: match regime {
            TlbRegime::L2Hit => 128,
            TlbRegime::Walk => 4096,
        }
        .min(buffers.sparse_capacity())
        .max(1),
        code_capacity: buffers.code_capacity(),
        main_pages: buffers.main().len() / PAGE,
    };
    let mut fitter = Fitter::new(threads, limits);
    fitter.proxy_misses = provenance
        .get("l3_misses_per_sec")
        .is_some_and(|p| p.method == Method::Proxy);
    let mut knobs = settings
        .knobs
        .clone()
        .or_else(|| settings.initial_knobs.clone())
        .unwrap_or_else(|| fitter.initial_knobs(&goal, regime));
    if let Err(e) = knobs.validate(&buffers) {
        bail!("knobs do not fit the buffers: {e}");
    }
    let fitting = settings.knobs.is_none() && !goal.is_empty();
    let window_s = window_seconds(settings.window_s);
    let refit_windows = ((REFIT_SECONDS / window_s).ceil() as u32).max(CONVERGED_WINDOWS);
    // Measured busy cores per second beside what the knobs intended for
    // that second, the duty times its factor.
    let mut rolling: std::collections::VecDeque<(f64, f64)> =
        std::collections::VecDeque::with_capacity(CORES_WINDOW_SECONDS);
    writeln!(
        progress,
        "run: {threads} threads, window {window_s:.1} s, busy-cores window {CORES_WINDOW_SECONDS} s, main buffer {} MiB{}",
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
    let mut prev_window: Option<(Knobs, Measured)> = None;
    let mut converged = false;
    let mut ever_converged = false;
    let mut streak = 0u32;
    let mut out_of_tolerance = 0u32;
    // The closest pass so far, to hold when the passes run out: the last
    // one is as likely to be a swing as a settling.
    let mut best: Option<(f64, Knobs, u32)> = None;
    let window_len = Duration::from_secs_f64(window_s);
    let deadline = settings
        .duration_s
        .map(|d| start + Duration::from_secs_f64(d));
    // Busy-cores ticks sit on wall-clock second boundaries, where the
    // workers redraw their duty factor; a tick straddling two seconds
    // would average two draws and halve the variance it measures.
    let mut next_cpu = Instant::now() + Duration::from_secs_f64(1.0 - unix_now().fract());
    let mut window_end = window.started + window_len;

    loop {
        let stop = stop_requested()
            || deadline.is_some_and(|d| Instant::now() >= d)
            || (settings.stop_when_converged && converged);
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
            // The tick follows a second boundary, so the sample is the
            // second just ended, whose factor the workers drew from it.
            let ended = (c.t as u64).saturating_sub(1);
            let intended = threads as f64
                * (knobs.duty * crate::loadgen::duty_factor(settings.seed, ended, knobs.cv))
                    .min(1.0);
            rolling.push_back((c.cores, intended));
            while rolling.len() > CORES_WINDOW_SECONDS {
                rolling.pop_front();
            }
            next_cpu =
                Instant::now() + Duration::from_secs_f64((1.0 - unix_now().fract()).max(0.05));
        }
        if now >= window_end || stop {
            let seconds = window.started.elapsed().as_secs_f64();
            if seconds >= window_s * 0.5 || windows.is_empty() {
                let sample = sampler.interval_tick(unix_now()).context("window sample")?;
                let accesses = accesses_total(&generator);
                let aps = (accesses - window.accesses_at_start) as f64 / seconds.max(1e-9);
                // Busy cores and burstiness: the seconds measured so far
                // against what the knobs intended for them give the
                // efficiency of the mean and the ratio of the burstiness,
                // which need no long-run average; applied to the clipped
                // model they predict the long-run values, and those are
                // judged.
                let (long, intended): (Vec<f64>, Vec<f64>) = rolling.iter().copied().unzip();
                let (cores_pred, cv_pred) = predicted_cores_cv(&long, &intended, &knobs, threads);
                let dram = sample
                    .values
                    .get("dram_bytes_per_sec")
                    .copied()
                    .or_else(|| dram_model(&sample, &knobs, aps));
                let mut m = measured_from(&sample, cv_pred, aps, dram);
                m.cv_samples = long.len();
                if cores_pred.is_some() {
                    m.cores = cores_pred;
                }
                let raw = m.clone();
                if let Some((k, p)) = &prev_window {
                    if *k == knobs {
                        m = blend(&m, p);
                    }
                }
                prev_window = Some((knobs.clone(), raw));
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
                            s.error
                                .map(|e| format!("({:+.1}%)", e * 100.0))
                                .unwrap_or_default()
                        ))
                        .collect::<Vec<_>>()
                        .join(" ")
                )?;
                windows.push(WindowRow {
                    end_s: start.elapsed().as_secs_f64(),
                    seconds,
                    pass: fitter.passes,
                    knobs: knobs.clone(),
                    accesses_per_s: aps,
                    instructions_per_access: m.instructions_per_access(),
                    values: statuses
                        .iter()
                        .map(|s| WindowValue {
                            key: s.key.into(),
                            measured: s.measured,
                            error: s.error,
                        })
                        .collect(),
                });
                // Busy cores and burstiness are not judged until their
                // rolling window is full: a lucky early reading of them
                // must not pick the pass to hold, and an unlucky one must
                // not keep a fit whose ratios are at target from
                // converging. They join the judgement when the window
                // fills, which is what the fitter does too.
                let judged: Vec<Status> = statuses
                    .iter()
                    .filter(|s| {
                        long.len() >= crate::fit::MIN_CV_SAMPLES
                            || !matches!(s.key, "cpu_usage_cores" | "cpu_usage_cv_1s")
                    })
                    .cloned()
                    .collect();
                streak = if Fitter::converged(&judged) {
                    out_of_tolerance = 0;
                    streak + 1
                } else {
                    out_of_tolerance += 1;
                    0
                };
                let distance = fitter.distance(&judged);
                if best.as_ref().is_none_or(|(d, _, _)| distance < *d) {
                    best = Some((distance, knobs.clone(), fitter.passes));
                }
                converged = streak >= CONVERGED_WINDOWS;
                ever_converged |= converged;
                if knobs.duty >= 1.0
                    && last_statuses_short_of_cores(&statuses)
                    && !notes.iter().any(|n| n.starts_with("busy cores"))
                {
                    notes.push(format!("busy cores fall short at full duty with {threads} threads; rerun with --threads above the target"));
                }
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
                // Once the fit has converged, a window or a few out of
                // tolerance is noise, a neighbour's burst or the recorder
                // attaching; the fit moves again only when a second of
                // them in a row is out. The memory of having converged
                // must not be the current streak, which the noisy window
                // has just reset.
                let refit = streak == 0 && (!ever_converged || out_of_tolerance >= refit_windows);
                // The pass limit is for a fit that has never converged;
                // one that has keeps following the host, since it moves
                // only when a second of windows in a row is out.
                if fitting && !stop && (settings.adaptive || refit) {
                    if fitter.passes < settings.max_passes || ever_converged {
                        let next = fitter.step(&knobs, &m, &goal);
                        if next != knobs {
                            generator.set_knobs(next.clone(), &buffers)?;
                            knobs = next;
                        }
                    } else if !notes.iter().any(|n| n.starts_with("not within tolerance")) {
                        // Out of passes: hold the closest pass, not the last.
                        let held = match &best {
                            Some((_, k, pass)) if *k != knobs => {
                                generator.set_knobs(k.clone(), &buffers)?;
                                knobs = k.clone();
                                format!("; holding the knobs of pass {pass}, the closest")
                            }
                            _ => "; holding".to_string(),
                        };
                        notes.push(format!(
                            "not within tolerance after {} passes{held}",
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

/// Whether the busy-cores target is measured and below tolerance.
fn last_statuses_short_of_cores(statuses: &[Status]) -> bool {
    statuses
        .iter()
        .any(|s| s.key == "cpu_usage_cores" && s.error.is_some_and(|e| e < -0.05))
}

fn generator_pins(pinned: Option<Vec<u32>>, threads: usize) -> Option<Vec<u32>> {
    pinned.map(|c| (0..threads).map(|i| c[i % c.len()]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn window_lengths() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(window_seconds(None), 0.1));
        assert!(close(window_seconds(Some(2.0)), 2.0));
        assert!(close(window_seconds(Some(0.0)), 0.05));
        assert_eq!(CORES_WINDOW_SECONDS, 40);
    }

    #[test]
    fn predicted_cores_and_cv_come_from_the_intended_seconds() {
        let k = Knobs {
            duty: 0.5,
            cv: 0.6,
            ..Knobs::default()
        };
        let (mean, cv) = crate::loadgen::clipped_duty_stats(0.5, 0.6);
        // Three seconds at 90 percent of what they intended: the mean
        // is corrected, the burstiness ratio is one.
        let intended = [0.8, 1.2, 1.0];
        let measured: Vec<f64> = intended.iter().map(|x| 0.9 * x).collect();
        let (c, v) = predicted_cores_cv(&measured, &intended, &k, 4);
        assert!((c.unwrap() - 0.9 * 4.0 * mean).abs() < 1e-9);
        assert!((v.unwrap() - cv).abs() < 1e-9);
        // Twice the intended spread doubles the predicted CV.
        let wider = [0.6, 1.4, 1.0];
        let (_, v) = predicted_cores_cv(&wider, &intended, &k, 4);
        assert!((v.unwrap() - 2.0 * cv).abs() < 1e-9);
        // No intended variation: the measured jitter is the burstiness.
        let flat = Knobs {
            duty: 0.5,
            cv: 0.0,
            ..Knobs::default()
        };
        let (c, v) = predicted_cores_cv(&[2.0, 2.0, 2.2], &[2.0, 2.0, 2.0], &flat, 4);
        assert!((c.unwrap() - 6.2 / 6.0 * 4.0 * 0.5).abs() < 1e-9);
        assert!(v.unwrap() > 0.0 && v.unwrap() < 0.1);
        assert_eq!(predicted_cores_cv(&[], &[], &flat, 4), (None, None));
    }

    #[test]
    fn the_sparse_set_moves_to_the_page_cache_file_when_anon_cannot_spare_it() {
        assert!(sparse_from_file(103, 16384, 1 << 30));
        assert!(
            !sparse_from_file(16384, 16384, 1 << 30),
            "anon has the pages"
        );
        assert!(
            !sparse_from_file(103, 16384, 1 << 20),
            "the file is too small"
        );
        assert!(!sparse_from_file(103, 16384, 0), "no file at all");
    }

    #[test]
    fn stream_capacities_follow_the_anonymous_target() {
        assert_eq!(stream_capacities(None, 16384, 16384), (16384, 16384));
        assert_eq!(stream_capacities(Some(0.0), 16384, 16384), (16384, 16384));
        assert_eq!(
            stream_capacities(Some(42.0e6), 16384, 16384),
            (1281, 1281),
            "an eighth of 42 MB in pages"
        );
        assert_eq!(
            stream_capacities(Some(1.0e5), 16384, 16384),
            (64, 64),
            "never below 64 pages"
        );
        assert_eq!(
            stream_capacities(Some(8.0e9), 16384, 100),
            (16384, 100),
            "settings cap too"
        );
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
    fn memory_limit_is_noted_not_enforced() {
        let mut t = Targets::new();
        assert_eq!(memory_limit_note(&t), None);
        t.set("memory_limit_bytes", 1.0e9).unwrap();
        assert_eq!(memory_limit_note(&t), None, "nothing asked for");
        t.set("anon_bytes", 6.0e8).unwrap();
        t.set("shmem_bytes", 3.0e8).unwrap();
        assert_eq!(
            memory_limit_note(&t),
            None,
            "components sum below the limit"
        );
        t.set("pagecache_bytes", 2.0e8).unwrap();
        let note = memory_limit_note(&t).unwrap();
        assert!(
            note.contains("1100000000 bytes") && note.contains("1000000000-byte"),
            "{note}"
        );
        t.set("memory_current_bytes", 9.0e8).unwrap();
        assert_eq!(
            memory_limit_note(&t),
            None,
            "the total, when given, is what counts"
        );
        t.set("memory_limit_bytes", 0.0).unwrap();
        t.set("memory_current_bytes", 9.0e9).unwrap();
        assert_eq!(memory_limit_note(&t), None, "a zero limit means none");
    }

    #[test]
    fn two_windows_at_one_setting_blend() {
        let a = Measured {
            ipc: Some(1.0),
            mpki: Some(4.0),
            dtlb_pki: Some(2.0),
            itlb_pki: None,
            dram: Some(2.0e9),
            accesses_per_s: 100.0,
            instructions_per_s: Some(1.0e9),
            cores: Some(2.0),
            cv: Some(0.1),
            cv_samples: 40,
        };
        let b = Measured {
            ipc: Some(2.0),
            mpki: Some(2.0),
            dtlb_pki: None,
            itlb_pki: Some(0.5),
            dram: Some(4.0e9),
            accesses_per_s: 300.0,
            instructions_per_s: Some(3.0e9),
            cores: Some(9.0),
            cv: Some(0.9),
            cv_samples: 39,
        };
        let m = blend(&a, &b);
        assert_eq!((m.ipc, m.mpki, m.dram), (Some(1.5), Some(3.0), Some(3.0e9)));
        assert_eq!(
            m.dtlb_pki,
            Some(2.0),
            "a side without a value keeps the other"
        );
        assert_eq!(m.itlb_pki, None, "nothing now, nothing");
        assert_eq!(
            (m.accesses_per_s, m.instructions_per_s),
            (200.0, Some(2.0e9))
        );
        assert_eq!(
            (m.cores, m.cv, m.cv_samples),
            (Some(2.0), Some(0.1), 40),
            "rolling-window fields are the window's own"
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
            ntlines: 2.0,
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
    fn a_run_without_counters_reports_its_targets_unverified() {
        let mut t = Targets::new();
        t.set("cpu_usage_cores", 1.0).unwrap();
        t.set("ipc", 1.0).unwrap();
        let settings = Settings {
            counters: false,
            knobs: Some(Knobs {
                duty: 0.2,
                sparse_pages: 1,
                code_pages: 1,
                ..Knobs::default()
            }),
            main_bytes: Some(1 << 20),
            sparse_capacity: 1,
            code_capacity: 1,
            duration_s: Some(0.3),
            window_s: Some(0.2),
            scratch_dir: std::env::temp_dir(),
            ..Settings::default()
        };
        let mut out = Vec::new();
        let report = run(&t, &settings, &mut out).unwrap();
        let ipc = report.targets.iter().find(|r| r.key == "ipc").unwrap();
        assert_eq!(ipc.tier, Tier::Unverified);
        assert_eq!(ipc.measured, None);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("counters not opened"))
        );
        let cores = report
            .targets
            .iter()
            .find(|r| r.key == "cpu_usage_cores")
            .unwrap();
        assert!(cores.measured.is_some(), "busy cores need no counters");
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
