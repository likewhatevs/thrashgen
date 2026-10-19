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
use crate::calibration::Calibration;
use crate::caps::{Expectations, Facts, Root};
use crate::cgroup::Cgroup;
use crate::cpu;
use crate::events::{Sysfs, Vendor, table_for};
use crate::fit::{Fitter, Goal, Limits, Measured, Status, TlbRegime};
use crate::footprint::{AnonBuffer, CacheFile, KernelFiles, ShmemBuffer, is_tmpfs};
use crate::loadgen::{Buffers, CacheRead, Generator, Geometry, Knobs, SparseSet};
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
    /// Pin when it buys per-core L3 attribution: AMD with `CAP_PERFMON`.
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
    /// Where a fit that converged is kept for the next run of the same
    /// targets on this CPU model; `None` keeps nothing.
    pub cache_path: Option<PathBuf>,
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
            cache_path: None,
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
    requested.map_or(0.1, |r| r.max(0.05))
}

/// Measured busy cores per second beside what the knobs intended for
/// that second, the duty times its factor, over a rolling window of
/// [`CORES_WINDOW_SECONDS`].
#[derive(Debug, Default)]
pub struct CoresWindow {
    pairs: std::collections::VecDeque<(f64, f64)>,
}

impl CoresWindow {
    /// Note one second's measured cores and what was intended for it.
    pub fn tick(&mut self, measured: f64, intended: f64) {
        self.pairs.push_back((measured, intended));
        while self.pairs.len() > CORES_WINDOW_SECONDS {
            self.pairs.pop_front();
        }
    }

    /// Seconds in the window.
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Whether no second has been measured yet.
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// The long-run busy cores and burstiness the window predicts; see
    /// [`predicted_cores_cv`].
    pub fn predict(&self, knobs: &Knobs, threads: usize) -> (Option<f64>, Option<f64>) {
        let (measured, intended): (Vec<f64>, Vec<f64>) = self.pairs.iter().copied().unzip();
        predicted_cores_cv(&measured, &intended, knobs, threads)
    }
}

/// The raw windows at the current knobs, newest last, with their
/// lengths, pooled for the fitter by [`pool`] over as many as
/// [`pool_count`] says; a change of knobs starts the pool afresh.
#[derive(Debug)]
pub struct Pool {
    recent: std::collections::VecDeque<(Knobs, Measured, f64)>,
    rate_sum: f64,
    rate_n: usize,
    seconds: f64,
}

impl Pool {
    /// A pool for a target with this burstiness.
    pub fn new(goal_cv: Option<f64>) -> Pool {
        Pool {
            recent: std::collections::VecDeque::with_capacity(POOL_WINDOWS_MAX),
            rate_sum: 0.0,
            rate_n: 0,
            seconds: pool_seconds(goal_cv),
        }
    }

    /// Add a window measured at `knobs` over `seconds` and return the
    /// pooled measurement the fitter sees.
    pub fn push(&mut self, knobs: &Knobs, window: Measured, seconds: f64) -> Measured {
        if self.recent.front().is_some_and(|(k, _, _)| k != knobs) {
            self.recent.clear();
        }
        if let Some(rate) = window.instructions_per_s {
            self.rate_sum += rate;
            self.rate_n += 1;
        }
        self.recent.push_back((knobs.clone(), window, seconds));
        while self.recent.len() > POOL_WINDOWS_MAX {
            self.recent.pop_front();
        }
        let mean_rate = if self.rate_n > 0 {
            self.rate_sum / self.rate_n as f64
        } else {
            0.0
        };
        let lengths: Vec<(f64, f64)> = self
            .recent
            .iter()
            .map(|(_, w, secs)| (w.instructions_per_s.unwrap_or(0.0), *secs))
            .collect();
        let n = pool_count(&lengths, mean_rate, self.seconds);
        let raws: Vec<Measured> = self
            .recent
            .iter()
            .skip(self.recent.len() - n)
            .map(|(_, w, _)| w.clone())
            .collect();
        pool(&raws)
    }
}

/// Where the fit stands: the streak of windows within tolerance, how
/// long it has been out, whether it has ever converged, and the closest
/// pass seen, to hold when the passes run out, since the last pass is
/// as likely to be a swing as a settling.
#[derive(Debug)]
pub struct Convergence {
    streak: u32,
    out_of_tolerance: u32,
    refit_windows: u32,
    /// A second out after converging has set the fit moving again, and
    /// it has not converged since.
    refitting: bool,
    /// Whether the last [`CONVERGED_WINDOWS`] windows were within.
    pub converged: bool,
    /// Whether the fit has converged at any point.
    pub ever: bool,
    /// Whether a frozen lever has already sent the fit to its closest pass.
    pub compromise: bool,
    /// Distance, knobs and pass of the closest pass.
    pub best: Option<(f64, Knobs, u32)>,
    /// Out of passes without converging, holding the closest pass.
    pub out_of_passes: bool,
    /// The targets within at that hold: a second of one of them out
    /// says the surroundings changed, and the hold gets a fresh budget.
    held_within: Vec<&'static str>,
    held_out: u32,
    /// Budgets granted after a hold saw its surroundings change.
    pub renewals: u32,
    /// A renewed budget is being spent and has not converged.
    renewing: bool,
    /// Passes of a re-fit since the closest pass last came closer.
    stale: u32,
    last_pass: u32,
}

/// Passes a re-fit may make without coming closer before it holds.
pub const STALE_PASSES: u32 = 6;

impl Convergence {
    /// A fit that re-fits after `refit_windows` consecutive windows out
    /// once it has converged.
    pub fn new(refit_windows: u32) -> Convergence {
        Convergence {
            streak: 0,
            out_of_tolerance: 0,
            refit_windows,
            refitting: false,
            converged: false,
            ever: false,
            compromise: false,
            best: None,
            out_of_passes: false,
            held_within: Vec::new(),
            held_out: 0,
            renewals: 0,
            renewing: false,
            stale: 0,
            last_pass: 0,
        }
    }

    /// A re-fit starts afresh: the closest pass is judged in the new
    /// surroundings, not against what the fit reached alone.
    fn start_over(&mut self) {
        self.best = None;
        self.stale = 0;
    }

    /// Whether a re-fit has stopped coming closer: beside a neighbour
    /// a target can be out of reach at any knobs, and a fit that keeps
    /// trying spends its budget moving. stress-ng's live fit made
    /// sixty-one moves over eight seconds beside a memory-bound
    /// neighbour before holding its closest pass, which was the right
    /// one (Zen 4, 2026-10-18). After [`STALE_PASSES`] passes without
    /// a closer one, a re-fit holds that pass; it moves again when its
    /// surroundings change, as any hold does.
    pub fn stuck(&self) -> bool {
        (self.refitting || self.renewing)
            && !self.converged
            && !self.out_of_passes
            && self.stale >= STALE_PASSES
    }

    /// Out of passes: hold, remembering which targets the hold meets.
    pub fn hold(&mut self, judged: &[Status]) {
        self.out_of_passes = true;
        self.renewing = false;
        self.stale = 0;
        self.held_within = judged
            .iter()
            .filter(|s| s.within && s.measured.is_some())
            .map(|s| s.key)
            .collect();
        self.held_out = 0;
    }

    /// Whether a held fit's surroundings have changed: a target the
    /// hold met has been out for a second of windows in a row. A fit
    /// held for an unreachable target, stress-ng's dTLB floor, kept its
    /// neighbour-time knobs after the neighbour left and ran at three
    /// times the IPC, since its budget was spent while the neighbour
    /// was there. Grants a fresh budget and says so, once per change.
    pub fn renew(&mut self, judged: &[Status]) -> bool {
        if !self.out_of_passes {
            return false;
        }
        let out = judged
            .iter()
            .any(|s| self.held_within.contains(&s.key) && s.measured.is_some() && !s.within);
        self.held_out = if out { self.held_out + 1 } else { 0 };
        if self.held_out < self.refit_windows {
            return false;
        }
        self.out_of_passes = false;
        self.held_within.clear();
        self.held_out = 0;
        self.renewals += 1;
        self.renewing = true;
        self.start_over();
        true
    }

    /// Note a window: whether its judged targets were all within, and
    /// its distance from tolerance at `knobs` after `pass` passes.
    pub fn observe(&mut self, within: bool, distance: f64, knobs: &Knobs, pass: u32) {
        if within {
            self.out_of_tolerance = 0;
            self.streak += 1;
        } else {
            self.out_of_tolerance += 1;
            self.streak = 0;
        }
        self.converged = self.streak >= CONVERGED_WINDOWS;
        self.ever |= self.converged;
        if self.converged {
            self.refitting = false;
            self.renewing = false;
        } else if self.ever && !self.refitting && self.out_of_tolerance >= self.refit_windows {
            self.refitting = true;
            self.start_over();
        }
        let closer = self.best.as_ref().is_none_or(|(d, _, _)| distance < *d);
        if closer {
            self.best = Some((distance, knobs.clone(), pass));
        }
        if pass != self.last_pass {
            self.stale = if closer { 0 } else { self.stale + 1 };
            self.last_pass = pass;
        }
    }

    /// Whether the fitter should move this window: out of tolerance,
    /// and, once converged, out for a second of windows in a row, since
    /// a window or a few out is noise, a neighbour's burst or the
    /// recorder attaching. The memory of having converged is not the
    /// current streak, which the noisy window has just reset.
    pub fn refit(&self) -> bool {
        self.streak == 0
            && (!self.ever || self.refitting || self.out_of_tolerance >= self.refit_windows)
    }

    /// Whether the fit counts as converged: it has converged and has
    /// not since been out for the second that sets it moving. The
    /// streak alone flickers when a target sits at the edge of its
    /// band, and a fit holding by design has not failed.
    pub fn holding(&self) -> bool {
        self.ever && !self.refitting
    }
}

/// Long-run busy cores and burstiness predicted from the seconds
/// measured so far. Each second's busy cores are compared with what the
/// knobs intended for it, so the mean's efficiency and the burstiness
/// ratio are known from a few seconds where a long-run average would
/// take as many seconds as the CV squared over the tolerance squared;
/// applied to the clipped model's mean and CV they give the values a
/// long recording would see. With no intended variation the measured
/// jitter is the burstiness. A second that intended under a tenth of
/// the model's mean carries the process's own overhead and little of
/// the knobs: fio's kernel-mode fit began in a plateau intending a
/// few thousandths of a core, read the overhead as a 23-fold
/// efficiency, cut its duty and spent two budgets of passes (Zen 4,
/// 2026-10-19); such seconds count for burstiness but not for the
/// mean, and until a telling second is in the mean is not judged.
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
    let telling = CORES_TELLING_SHARE * threads as f64 * model_mean;
    let (sum_meas, sum_int) = measured
        .iter()
        .zip(intended)
        .filter(|(_, i)| **i >= telling)
        .fold((0.0, 0.0), |(m, i), (x, y)| (m + x, i + y));
    let cores = if sum_int > 0.0 {
        Some(sum_meas / sum_int * threads as f64 * model_mean)
    } else if intended.iter().all(|i| *i <= 0.0) {
        Some(measured.iter().sum::<f64>() / measured.len() as f64)
    } else {
        None
    };
    let one =
        |xs: &[f64]| pooled_cv(std::slice::from_ref(&xs.to_vec()), xs.len()).map(|(cv, _)| cv);
    let cv = match (one(measured), one(intended)) {
        (Some(m), Some(i)) if i > 1e-9 => Some(model_cv * m / i),
        (m, _) => m,
    };
    (cores, cv)
}

/// The share of the model's mean busy cores a second must have
/// intended to count toward the efficiency of the mean.
pub const CORES_TELLING_SHARE: f64 = 0.1;

/// How long the pooled windows must be, in instructions at the run's
/// mean rate: 0.4 s, and two seconds more per unit of the burstiness
/// target, since a bursty duty cycle's ratios differ from one second
/// to the next, a quiet second's few instructions carrying the misses
/// of a cold cache and the kernel's wakeups, and a recorder's interval
/// averages several of them.
pub fn pool_seconds(goal_cv: Option<f64>) -> f64 {
    0.4 + 2.0 * goal_cv.unwrap_or(0.0).clamp(0.0, 2.0)
}
/// Windows kept for pooling at most.
pub const POOL_WINDOWS_MAX: usize = 60;

/// How many of the newest windows at the same knobs, each with its
/// instruction rate and length, are pooled: at least two, then as many
/// as it takes to hold `seconds` of instructions at the run's mean
/// rate.
pub fn pool_count(windows: &[(f64, f64)], mean_rate: f64, seconds: f64) -> usize {
    let budget = seconds * mean_rate;
    let mut held = 0.0;
    for (n, (rate, length)) in windows.iter().rev().enumerate() {
        held += rate * length;
        if n + 1 >= 2 && held >= budget {
            return n + 1;
        }
    }
    windows.len()
}

/// Consecutive windows at the same knobs pooled as one longer window:
/// the rates per instruction weighted by the instructions of each, IPC
/// as the instructions over the cycles they took, and the per-second
/// rates averaged, so a single noisy window, a neighbour's burst, a
/// page-cache refill or a quiet second of a bursty duty cycle, moves
/// nothing on its own. Busy cores and burstiness come from the rolling
/// window already and are the newest window's.
pub fn pool(windows: &[Measured]) -> Measured {
    let newest = windows.last().expect("a window to pool").clone();
    if windows.len() == 1 {
        return newest;
    }
    let weight = |w: &Measured| w.instructions_per_s.unwrap_or(w.accesses_per_s).max(0.0);
    let weighted = |pick: fn(&Measured) -> Option<f64>| {
        let (sum, total) = windows
            .iter()
            .filter_map(|w| pick(w).map(|v| (v, weight(w))))
            .fold((0.0, 0.0), |(s, t), (v, wt)| (s + v * wt, t + wt));
        (total > 0.0).then(|| sum / total)
    };
    let mean = |pick: fn(&Measured) -> Option<f64>| {
        let vals: Vec<f64> = windows.iter().filter_map(pick).collect();
        (!vals.is_empty()).then(|| vals.iter().sum::<f64>() / vals.len() as f64)
    };
    let ipc = {
        let (instr, cycles) = windows
            .iter()
            .filter_map(|w| {
                w.ipc
                    .filter(|i| *i > 0.0)
                    .map(|i| (weight(w), weight(w) / i))
            })
            .fold((0.0, 0.0), |(a, b), (i, c)| (a + i, b + c));
        (cycles > 0.0).then_some(instr / cycles)
    };
    Measured {
        ipc,
        mpki: weighted(|w| w.mpki),
        dtlb_pki: weighted(|w| w.dtlb_pki),
        itlb_pki: weighted(|w| w.itlb_pki),
        dram: mean(|w| w.dram),
        instructions_per_s: mean(|w| w.instructions_per_s),
        factor: mean(|w| w.factor),
        accesses_per_s: mean(|w| Some(w.accesses_per_s)).unwrap_or(0.0),
        ..newest
    }
}

/// Rolling window of one-second samples behind busy cores and
/// burstiness: ten factors, so the ratio sees half a block, and a
/// neighbour's arrival is in within a minute.
pub const CORES_WINDOW_SECONDS: usize = 2 * crate::loadgen::FACTOR_BLOCK as usize;

/// Whether the sparse stream should run on the page-cache file: the
/// anonymous target cannot spare the pages the configured sparse set
/// needs, and the file has them. Pages visited there count as page
/// cache, which the profile asked for anyway.
pub fn sparse_from_file(
    anon_cap: usize,
    configured: usize,
    pagecache_bytes: u64,
    page: usize,
) -> bool {
    anon_cap < configured && pagecache_bytes >= (configured * page) as u64
}

/// Whether a `cgroup.procs` listing names `pid` and nothing else.
pub fn alone_in_cgroup(procs: &str, pid: u32) -> bool {
    let mut pids = procs.split_whitespace().peekable();
    pids.peek().is_some() && pids.all(|p| p.parse::<u32>().is_ok_and(|x| x == pid))
}

/// Whether page-cache reads are a lever: the file must be larger than
/// the L3, or the kernel copies it from cache and a read moves no DRAM
/// bytes and misses nothing, leaving only the instructions that dilute
/// MPKI. Without a known L3 size they are not.
pub fn page_cache_reads_usable(file_bytes: u64, l3_bytes: Option<u64>) -> bool {
    l3_bytes.is_some_and(|l3| file_bytes > l3)
}

/// Pages to reserve for the sparse and code streams: the configured
/// capacities, each capped at an eighth of the anonymous target or a
/// quarter of the anonymous field's absolute tolerance, whichever is
/// more, so a small footprint is not swamped by the streams' own pages
/// and a tiny one still gets a code set past the iTLB's reach: fio's
/// 3.4 MB of anonymous memory allowed 103 stub pages, which the L2
/// iTLB holds, and ten thousand calls an access made a tenth of the
/// iTLB rate the recording asked for (Zen 4, 2026-10-18).
pub fn stream_capacities(
    anon_target: Option<f64>,
    sparse: usize,
    code: usize,
    page: usize,
) -> (usize, usize) {
    match anon_target {
        Some(anon) if anon > 0.0 => {
            let floor = crate::fit::absolute_tolerance("anon_bytes") / 4.0;
            let cap = ((anon / 8.0).max(floor) as usize / page).max(64);
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
    page: usize,
) -> MemoryPlan {
    let stream_pages = ((sparse_pages + code_pages) * page) as u64;
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
/// of the main stream written back, a line of `line` bytes each.
pub fn dram_model(sample: &Sample, knobs: &Knobs, accesses_per_s: f64, line: usize) -> Option<f64> {
    let misses = sample.values.get("l3_misses_per_sec")?;
    Some(line as f64 * (misses + accesses_per_s * (knobs.ntlines + knobs.wfrac * knobs.lines)))
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
        factor: None,
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
/// pair up once every core is in use, and the fastest cores first
/// where the host's cores differ, as the scheduler places a busy
/// thread: on an Apple chip the E cores are numbered first, and a
/// worker pinned to one runs at half the clock of the P core a
/// recorded benchmark ran on.
pub fn choose_cpus(affinity: &[u32], topo: &Topology, n: usize) -> Vec<u32> {
    let mut first: Vec<u32> = Vec::new();
    let mut rest: Vec<u32> = Vec::new();
    for &c in affinity {
        match topo.cpu(c) {
            Some(cpu) if cpu.siblings.iter().find(|s| affinity.contains(s)) == Some(&c) => {
                first.push(c);
            }
            _ => rest.push(c),
        }
    }
    let capacity = |c: &u32| std::cmp::Reverse(topo.cpu(*c).and_then(|x| x.capacity).unwrap_or(0));
    first.sort_by_key(capacity);
    rest.sort_by_key(capacity);
    first.extend(rest);
    first.into_iter().take(n).collect()
}

/// The memory the sampler should see as the process's page cache.
fn cache_handles(cache: Option<&CacheFile>) -> Vec<File> {
    cache
        .into_iter()
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
    // The line a miss fetches: the table's where it knows better than
    // sysfs, else what sysfs reports, else the common 64 bytes.
    let line = table
        .and_then(|t| t.memory_line)
        .or_else(|| topo.line_bytes().map(|b| b as usize))
        .unwrap_or(64);
    let demand_only = table.is_some_and(|t| t.demand_misses_only);
    let geo = Geometry::host(line).runs_in_pages(demand_only);
    notes.push(format!(
        "streams laid out on {}-byte pages and {}-byte lines{}",
        geo.page,
        geo.line,
        if geo.run_in_pages {
            ", a run's lines each in a page of their own"
        } else {
            ""
        }
    ));

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

    // Memory and busy time from the tool's own cgroup when it has one
    // to itself, as in a container or a scope of its own: the cgroup
    // charges the slab of the empty files, the page tables and the
    // threads' stacks, which the process view does not see and which
    // put twelve instances a fifth over their memory target, and it is
    // what a recorder of the scope reads. Shared with other processes,
    // a login session's scope say, it would count theirs too.
    let scope = match &facts.cgroup {
        Some(c) if c.memory_stat && c.cpu_stat && !c.path.is_empty() => {
            let procs =
                std::fs::read_to_string(root.path(&c.mount).join(&c.path).join("cgroup.procs"))
                    .unwrap_or_default();
            if alone_in_cgroup(&procs, std::process::id()) {
                notes.push(format!(
                    "memory and busy time from the cgroup {}, which this process has to itself",
                    c.path
                ));
                Scope::Cgroup(Cgroup::new(&root, &c.mount, &c.path))
            } else {
                Scope::SelfProcess
            }
        }
        _ => Scope::SelfProcess,
    };
    // Footprint first, so its page faults do not land in a window.
    let baseline = measure::memory(&scope, &root, &[]).context("baseline memory")?;
    let (sparse_capacity, code_capacity) = stream_capacities(
        targets.get("anon_bytes"),
        settings.sparse_capacity,
        settings.code_capacity,
        geo.page,
    );
    let mem_plan = plan_memory(
        targets,
        baseline.anon.unwrap_or(0),
        sparse_capacity,
        code_capacity,
        settings.main_bytes,
        geo.page,
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
            geo.page,
        )
    });
    let sparse = match on_file {
        Some(c) => {
            notes.push(format!(
                "sparse stream on the page-cache file: {} pages, which the anonymous footprint could not spare",
                settings.sparse_capacity
            ));
            SparseSet::File(
                c.map(settings.sparse_capacity * geo.page)
                    .context("map the page-cache file for the sparse stream")?,
            )
        }
        None => SparseSet::Anon(
            crate::footprint::AnonBuffer::new(sparse_capacity.max(1) * geo.page, false)
                .context("allocate the sparse buffer")?,
        ),
    };
    let buffers = Arc::new(
        Buffers::with_sparse(
            geo,
            mem_plan.main_bytes as usize,
            settings.huge,
            sparse,
            code_capacity,
            // A page-cache target below a page leaves nothing to read.
            cache
                .as_ref()
                .filter(|c| c.len() >= geo.page as u64)
                .map(|c| CacheRead::new(c.file(), geo.page))
                .transpose()
                .context("open the page-cache file for reads")?,
        )
        .context("allocate the main, sparse and code buffers")?,
    );
    let _shmem = if mem_plan.shmem_bytes > 0 {
        Some(ShmemBuffer::new(mem_plan.shmem_bytes as usize).context("create shared memory")?)
    } else {
        None
    };
    let mut kernel = None;
    if mem_plan.kernel_files > 0 {
        let mut k = KernelFiles::new(&settings.scratch_dir).context("create the slab directory")?;
        k.set_count(mem_plan.kernel_files)
            .context("create slab files")?;
        notes.push(format!(
            "kernel memory: {} empty files at {} bytes each to start; the count follows the measured total",
            mem_plan.kernel_files,
            crate::footprint::BYTES_PER_EMPTY_FILE
        ));
        kernel = Some(k);
    }
    let mut last_kernel_adjust = Instant::now();
    let mut extra_anon: Vec<AnonBuffer> = Vec::new();

    // Bandwidth: a resctrl group with this thread in it, inherited by the workers.
    let bandwidth: Option<MonGroup> = if let Some(rc) = Resctrl::from_facts(&root, &facts) {
        match rc
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
        }
    } else {
        notes.push("DRAM bandwidth modelled: resctrl is not mounted with mbm_total_bytes".into());
        None
    };
    let bandwidth_measured = bandwidth.is_some();

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
        scope.clone(),
        root.clone(),
        plan.fields.clone(),
        counters,
        bandwidth,
        cache_handles(cache.as_ref()),
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
    // A modelled bandwidth is reported, not fitted: the model knows
    // nothing the counted misses do not, and fitting it against a
    // recording set non-temporal lines by formula and took IPC, MPKI
    // and the TLB rates with them.
    let dram_modelled = !bandwidth_measured && goal.dram.is_some();
    let mut fit_goal = goal.clone();
    if dram_modelled {
        fit_goal.dram = None;
        notes.push("DRAM bandwidth is reported from the model and not fitted".into());
    }
    let regime = if vendor == Some(Vendor::Amd) {
        TlbRegime::L2Hit
    } else {
        TlbRegime::Walk
    };
    let kreads_usable = cache
        .as_ref()
        .is_some_and(|c| page_cache_reads_usable(c.len(), topo.l3_bytes()));
    if let (Some(c), false) = (cache.as_ref(), kreads_usable) {
        notes.push(match topo.l3_bytes() {
            Some(l3) => format!(
                "page-cache reads are not a lever: the page-cache file ({} KiB) fits in the L3 ({} KiB)",
                c.len() >> 10,
                l3 >> 10
            ),
            None => "page-cache reads are not a lever: the L3 size is not in sysfs".to_string(),
        });
    }
    let limits = Limits {
        sparse_capacity: buffers.sparse_capacity(),
        sparse_floor: match regime {
            TlbRegime::L2Hit => 128,
            TlbRegime::Walk => 4096,
        }
        .min(buffers.sparse_capacity())
        .max(1),
        code_capacity: buffers.code_capacity(),
        main_pages: buffers.main().len() / geo.page,
        kreads_available: buffers.has_cache() && kreads_usable,
        geo,
    };
    let mut fitter = Fitter::new(threads, limits);
    fitter.proxy_misses = demand_only
        || provenance
            .get("l3_misses_per_sec")
            .is_some_and(|p| p.method == Method::Proxy);
    fitter.kernel_counted = provenance
        .get("l3_misses_per_sec")
        .is_some_and(|p| p.mode == Mode::All);
    let mut knobs = settings
        .knobs
        .clone()
        .or_else(|| settings.initial_knobs.clone())
        .unwrap_or_else(|| fitter.initial_knobs(&fit_goal, regime));
    if let Err(e) = knobs.validate(&buffers) {
        bail!("knobs do not fit the buffers: {e}");
    }
    let fitting = settings.knobs.is_none() && !goal.is_empty();
    let window_s = window_seconds(settings.window_s);
    let refit_windows = ((REFIT_SECONDS / window_s).ceil() as u32).max(CONVERGED_WINDOWS);
    let mut cores_window = CoresWindow::default();
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
    let mut pool = Pool::new(goal.cv);
    let mut fit = Convergence::new(refit_windows);
    let mut last_move = Instant::now();
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
            || (settings.stop_when_converged && fit.converged);
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
            // While the page-cache file is being written its writer's
            // CPU is in the busy cores and the workers' knobs did not
            // intend it: a gigabyte took the first two seconds and
            // read as a core where a bursty profile intended a few
            // hundredths, and the fit cut duty by tens.
            if cache.as_ref().is_none_or(CacheFile::written) {
                cores_window.tick(c.cores, intended);
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
                let (cores_pred, cv_pred) = cores_window.predict(&knobs, threads);
                let dram = sample
                    .values
                    .get("dram_bytes_per_sec")
                    .copied()
                    .or_else(|| dram_model(&sample, &knobs, aps, geo.line));
                let mut m = measured_from(&sample, cv_pred, aps, dram);
                m.cv_samples = cores_window.len();
                if !cores_window.is_empty() {
                    m.cores = cores_pred;
                }
                // The factor of the second the window's midpoint fell in,
                // clipped the way the workers clip it.
                m.factor = Some(if knobs.cv > 0.0 && knobs.duty > 0.0 {
                    let second = (unix_now() - seconds / 2.0).floor() as u64;
                    crate::loadgen::duty_factor(settings.seed, second, knobs.cv)
                        .min(1.0 / knobs.duty)
                } else {
                    1.0
                });
                let m = pool.push(&knobs, m, seconds);
                // The report keeps what was measured; the fit is judged at
                // the long-run factor once the wakeup costs are known.
                let measured_statuses = fitter.statuses(&m, &goal);
                fitter.wake.learn(&knobs, &m);
                let m = fitter.wake.long_run(&knobs, &m);
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
                            s.measured.map_or_else(|| "-".into(), |v| format!("{v:.4}")),
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
                    factor: m.factor,
                    values: measured_statuses
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
                    .filter(|s| crate::fit::judged(s.key, cores_window.len()))
                    .filter(|s| !(dram_modelled && s.key == "dram_bytes_per_sec"))
                    .cloned()
                    .collect();
                fit.observe(
                    Fitter::converged(&judged),
                    fitter.distance(&judged),
                    &knobs,
                    fitter.passes,
                );
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
                    if c.resident_bytes().is_ok_and(|r| r < c.len() * 95 / 100) {
                        let _ = c.reread();
                    }
                }
                // Kernel memory: the empty files' slab cost is an
                // estimate, so what the measured total stands above or
                // below the target corrects their count, damped and once
                // a second so the slab settles between moves. Twelve
                // instances on one host ran a fifth over the estimate.
                if let (Some(k), Some(target), Some(total)) = (
                    kernel.as_mut(),
                    targets.get("memory_current_bytes"),
                    sample.values.get("memory_current_bytes"),
                ) {
                    let off = total - target;
                    let slack =
                        crate::fit::absolute_tolerance("memory_current_bytes").max(0.05 * target);
                    if off.abs() > slack && last_kernel_adjust.elapsed().as_secs_f64() >= 1.0 {
                        let per_file = crate::footprint::BYTES_PER_EMPTY_FILE as f64;
                        let files = (k.count() as f64 - 0.5 * off / per_file).max(0.0) as usize;
                        if files != k.count() {
                            k.set_count(files).context("adjust the slab files")?;
                            last_kernel_adjust = Instant::now();
                        }
                    }
                }
                last_sample = Some(sample);
                // The pass limit is for a fit that has never converged;
                // one that has keeps following the host, since it moves
                // only when a second of windows in a row is out.
                if fitting && !stop && fit.renew(&judged) {
                    notes.push(format!(
                        "a target the hold met has been out for a second after {} passes; fitting again with a fresh budget",
                        fitter.passes
                    ));
                }
                let budget = settings.max_passes.saturating_mul(fit.renewals + 1);
                if fitting && !stop && (settings.adaptive || fit.refit()) {
                    if fit.stuck() {
                        // A re-fit that has stopped coming closer: what it
                        // cannot reach, it cannot reach at these knobs
                        // either, so it holds the closest pass it saw.
                        let held = match &fit.best {
                            Some((_, k, pass)) if *k != knobs => {
                                generator.set_knobs(k.clone(), &buffers)?;
                                knobs = k.clone();
                                format!("; holding the knobs of pass {pass}, the closest")
                            }
                            _ => "; holding".to_string(),
                        };
                        notes.push(format!(
                            "no closer after {STALE_PASSES} passes of re-fitting; what is out is out of reach here{held}"
                        ));
                        fit.hold(&judged);
                    } else if fitter.passes < budget || fit.ever {
                        let mut next = fitter.step(&knobs, &m, &fit_goal);
                        // A lever the IPC and MPKI rules pulled against
                        // each other has just been frozen: the two targets
                        // meet nowhere along it, so the fit goes back to
                        // the closest pass it saw and continues from there.
                        let frozen = fitter.lines_track.frozen
                            || fitter.seq_track.frozen
                            || fitter.ops_track.frozen;
                        if frozen && !fit.compromise {
                            fit.compromise = true;
                            if let Some((_, k, pass)) = &fit.best {
                                next = k.clone();
                                notes.push(format!(
                                    "a lever has turned twice within three passes between what two targets ask; holding pass {pass}, the closest, as the compromise"
                                ));
                            }
                        }
                        if fitter.mpki_at_ceiling
                            && !notes.iter().any(|n| n.starts_with("L3 MPKI short"))
                        {
                            notes.push("L3 MPKI short at one op per access with the run length spent: the misses per instruction are at the generator's ceiling for this profile".to_string());
                        }
                        if next != knobs {
                            generator.set_knobs(next.clone(), &buffers)?;
                            knobs = next;
                            last_move = Instant::now();
                        }
                    } else if !fit.out_of_passes {
                        // Out of passes: hold the closest pass, not the last.
                        let held = match &fit.best {
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
                        fit.hold(&judged);
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
    // A fit that ends out of tolerance while still moving reports the
    // closest pass it saw, which is what a hold of its knobs should
    // run, since the last pass is then as likely a swing as a settling;
    // one that has stood still for a second has settled where it is.
    if fitting && !fit.holding() && last_move.elapsed().as_secs_f64() < REFIT_SECONDS {
        if let Some((_, k, pass)) = &fit.best {
            if *k != knobs {
                knobs = k.clone();
                notes.push(format!(
                    "ended out of tolerance; the knobs reported are those of pass {pass}, the closest"
                ));
            }
        }
    }
    notes.extend(fitter.wake.note());
    let report = Report {
        cpu: ident,
        table: table.map(|t| t.table.to_string()),
        threads,
        pinned_cpus: generator_pins(pinned, threads),
        knobs,
        main_bytes: mem_plan.main_bytes,
        thp_bytes,
        passes: fitter.passes,
        converged: fit.holding(),
        targets: target_rows,
        memory: memory_rows,
        windows,
        notes,
    };
    // A fit that converged is kept for the next run of these targets on
    // this CPU model, which then starts where this one ended.
    if let Some(path) = settings.cache_path.as_ref().filter(|_| fitting && fit.ever) {
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(anyhow::Error::from)
            .and_then(|()| Calibration::from_report(targets, &report).save(path));
        match written {
            Ok(()) => writeln!(progress, "cached    fit kept at {}", path.display())?,
            Err(e) => writeln!(progress, "note      fit not cached: {e:#}")?,
        }
    }
    Ok(report)
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
    fn convergence_needs_a_streak_and_refits_after_a_second_out() {
        let k = Knobs::default();
        let mut c = Convergence::new(10);
        assert!(c.refit(), "never converged: every window out moves");
        c.observe(true, 0.5, &k, 1);
        assert!(!c.converged && !c.refit());
        c.observe(true, 0.2, &k, 1);
        assert!(c.converged && c.ever && c.holding());
        assert_eq!(c.best.as_ref().map(|(d, _, p)| (*d, *p)), Some((0.2, 1)));
        for i in 0..9 {
            c.observe(false, 3.0, &k, 2);
            assert!(!c.converged && c.ever);
            assert!(!c.refit(), "window {i} out: not yet");
            assert!(c.holding(), "window {i} out: still holding by design");
        }
        c.observe(false, 3.0, &k, 2);
        assert!(c.refit(), "a second out: the fit moves");
        assert!(!c.holding(), "a second out: no longer converged");
        c.observe(true, 0.9, &k, 3);
        assert!(
            !c.refit() && !c.converged,
            "one within: nothing to move for"
        );
        assert!(!c.holding(), "one within is not converged again");
        assert_eq!(
            c.best.as_ref().map(|(_, _, p)| *p),
            Some(3),
            "the closest is judged afresh in the new surroundings"
        );
        // Moving again, the fit is as at first: a window out moves it
        // without another second's wait, or a target left at the edge
        // of its band would neither move nor converge.
        c.observe(false, 1.1, &k, 3);
        assert!(c.refit(), "out while re-fitting: moves at once");
        c.observe(true, 0.9, &k, 3);
        c.observe(true, 0.9, &k, 3);
        assert!(c.converged && c.holding(), "two within: converged again");
        // A target at the edge of its band flickers: one window out
        // and back leaves the fit holding and counted as converged.
        c.observe(false, 1.1, &k, 3);
        c.observe(true, 0.9, &k, 3);
        assert!(!c.converged && c.holding());
    }

    #[test]
    fn a_refit_that_stops_coming_closer_holds_and_moves_again_on_a_change() {
        let status = |key: &'static str, within: bool| Status {
            key,
            target: 1.0,
            measured: Some(if within { 1.0 } else { 2.0 }),
            error: Some(if within { 0.0 } else { 1.0 }),
            within,
        };
        let k = Knobs::default();
        let mut c = Convergence::new(10);
        // A cold fit that makes no progress is not stuck: that is what
        // the pass budget is for.
        for pass in 1..=12 {
            c.observe(false, 5.0, &k, pass);
        }
        assert!(!c.stuck());
        // Converged, then out for a second: a re-fit, judged afresh.
        c.observe(true, 0.1, &k, 13);
        c.observe(true, 0.1, &k, 13);
        assert!(c.converged);
        for _ in 0..10 {
            c.observe(false, 4.0, &k, 13);
        }
        assert!(
            c.refit() && c.best.as_ref().is_some_and(|(d, _, _)| *d == 4.0),
            "{:?}",
            c.best
        );
        // Coming closer pass by pass: not stuck.
        for (i, d) in [3.0, 2.5, 2.0, 1.8, 1.6, 1.5, 1.4].iter().enumerate() {
            c.observe(false, *d, &k, 14 + i as u32);
            assert!(!c.stuck(), "pass {} came closer", 14 + i);
        }
        // Then no closer for six passes: stuck, and the closest pass is
        // the one to hold.
        for i in 0..STALE_PASSES {
            assert!(!c.stuck());
            c.observe(false, 1.5, &k, 21 + i);
        }
        assert!(c.stuck());
        assert_eq!(c.best.as_ref().map(|(d, _, p)| (*d, *p)), Some((1.4, 20)));
        c.hold(&[status("ipc", false), status("l3_mpki", true)]);
        assert!(!c.stuck() && c.out_of_passes);
        // The surroundings change: a target the hold met goes out for a
        // second, and the re-fit starts afresh with a fresh budget.
        for _ in 0..10 {
            c.renew(&[status("ipc", false), status("l3_mpki", false)]);
        }
        assert!(!c.out_of_passes && c.renewals == 1 && c.best.is_none());
        c.observe(false, 0.9, &k, 27);
        assert!(!c.stuck());
    }

    #[test]
    fn a_hold_out_of_passes_gets_a_fresh_budget_when_what_it_met_goes_out() {
        let status = |key: &'static str, within: bool| Status {
            key,
            target: 1.0,
            measured: Some(if within { 1.0 } else { 2.0 }),
            error: Some(if within { 0.0 } else { 1.0 }),
            within,
        };
        let mut c = Convergence::new(10);
        let k = Knobs::default();
        c.observe(false, 2.0, &k, 36);
        assert!(!c.renew(&[status("ipc", false)]), "not holding yet");
        c.hold(&[status("ipc", true), status("dtlb_pki", false)]);
        assert!(c.out_of_passes);
        for _ in 0..20 {
            assert!(
                !c.renew(&[status("ipc", true), status("dtlb_pki", false)]),
                "the unreachable target stays out: no change"
            );
        }
        for i in 0..9 {
            assert!(
                !c.renew(&[status("ipc", false), status("dtlb_pki", false)]),
                "{i}"
            );
        }
        assert!(
            !c.renew(&[status("ipc", true), status("dtlb_pki", false)]),
            "one within resets"
        );
        for _ in 0..9 {
            c.renew(&[status("ipc", false), status("dtlb_pki", false)]);
        }
        assert!(
            c.renew(&[status("ipc", false), status("dtlb_pki", false)]),
            "a second out"
        );
        assert!(!c.out_of_passes && c.renewals == 1);
        assert!(!c.renew(&[status("ipc", false)]), "once per hold");
    }

    #[test]
    fn page_cache_reads_need_a_file_larger_than_the_l3() {
        assert!(page_cache_reads_usable(1 << 30, Some(32 << 20)));
        assert!(!page_cache_reads_usable(4096, Some(32 << 20)));
        assert!(!page_cache_reads_usable(32 << 20, Some(32 << 20)));
        assert!(!page_cache_reads_usable(1 << 30, None));
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
        // A deep-quiet plateau intends a few thousandths of a core and
        // reads the process's overhead: no mean until a telling second
        // is in, and then the telling seconds alone set it.
        let bursty = Knobs {
            duty: 0.26,
            cv: 1.3,
            ..Knobs::default()
        };
        let (model_mean, _) = crate::loadgen::clipped_duty_stats(0.26, 1.3);
        let (c, v) = predicted_cores_cv(&[0.06, 0.06], &[0.0026, 0.0026], &bursty, 1);
        assert_eq!(c, None, "{c:?}");
        assert!(v.is_some(), "burstiness still has its ratio");
        let (c, _) = predicted_cores_cv(&[0.06, 0.06, 0.27], &[0.0026, 0.0026, 0.26], &bursty, 1);
        assert!(
            (c.unwrap() - 0.27 / 0.26 * model_mean).abs() < 1e-9,
            "{c:?}"
        );
        // Nothing intended at all: the measured mean is what there is,
        // and its jitter the burstiness.
        let idle = Knobs {
            duty: 0.0,
            cv: 0.0,
            ..Knobs::default()
        };
        let (c, v) = predicted_cores_cv(&[0.02, 0.04], &[0.0, 0.0], &idle, 1);
        assert!(
            (c.unwrap() - 0.03).abs() < 1e-12 && v.is_some(),
            "{c:?} {v:?}"
        );
    }

    #[test]
    fn the_sparse_set_moves_to_the_page_cache_file_when_anon_cannot_spare_it() {
        const PAGE: usize = 4096;
        assert!(sparse_from_file(103, 16384, 1 << 30, PAGE));
        assert!(
            !sparse_from_file(16384, 16384, 1 << 30, PAGE),
            "anon has the pages"
        );
        assert!(
            !sparse_from_file(103, 16384, 1 << 20, PAGE),
            "the file is too small"
        );
        assert!(!sparse_from_file(103, 16384, 0, PAGE), "no file at all");
        assert!(
            !sparse_from_file(103, 16384, 1 << 26, 16384),
            "64 MiB holds 16384 pages of 4 KiB, not of 16 KiB"
        );
    }

    #[test]
    fn stream_capacities_follow_the_anonymous_target() {
        const PAGE: usize = 4096;
        assert_eq!(stream_capacities(None, 16384, 16384, PAGE), (16384, 16384));
        assert_eq!(
            stream_capacities(Some(0.0), 16384, 16384, PAGE),
            (16384, 16384)
        );
        assert_eq!(
            stream_capacities(Some(42.0e6), 16384, 16384, 16384),
            (320, 320),
            "an eighth of 42 MB in 16 KiB pages"
        );
        assert_eq!(
            stream_capacities(Some(42.0e6), 16384, 16384, PAGE),
            (1281, 1281),
            "an eighth of 42 MB in pages"
        );
        assert_eq!(
            stream_capacities(Some(1.0e5), 16384, 16384, PAGE),
            (512, 512),
            "a tiny target gets a quarter of the memory tolerance, 2 MiB"
        );
        assert_eq!(
            stream_capacities(Some(1.0e5), 16384, 16384, 16384),
            (128, 128),
            "the same 2 MiB in 16 KiB pages"
        );
        assert_eq!(
            stream_capacities(Some(1.0e5), 16384, 16384, 1 << 20),
            (64, 64),
            "never below 64 pages"
        );
        assert_eq!(
            stream_capacities(Some(8.0e9), 16384, 100, PAGE),
            (16384, 100),
            "settings cap too"
        );
    }

    #[test]
    fn memory_planning() {
        const PAGE: usize = 4096;
        let mut t = Targets::new();
        let p = plan_memory(&t, 10 << 20, 100, 100, None, PAGE);
        assert_eq!(
            p,
            MemoryPlan {
                main_bytes: DEFAULT_MAIN_BYTES,
                pagecache_bytes: 0,
                shmem_bytes: 0,
                kernel_files: 0
            }
        );
        t.set("anon_bytes", 100.0 * f64::from(1 << 20)).unwrap();
        let p = plan_memory(&t, 10 << 20, 1024, 1024, None, PAGE);
        assert_eq!(p.main_bytes, (100 << 20) - (10 << 20) - 2048 * PAGE as u64);
        t.set("anon_bytes", 1.0).unwrap();
        assert_eq!(
            plan_memory(&t, 10 << 20, 1, 1, None, PAGE).main_bytes,
            MIN_MAIN_BYTES,
            "floored"
        );
        assert_eq!(
            plan_memory(&t, 0, 1, 1, Some(5 << 20), PAGE).main_bytes,
            5 << 20,
            "override wins"
        );
        t.set("pagecache_bytes", 3.0e6).unwrap();
        t.set("shmem_bytes", 2.0e6).unwrap();
        t.set("memory_current_bytes", 5.0e6 + 1.0 + 1_100_000.0)
            .unwrap();
        let p = plan_memory(&t, 0, 1, 1, None, PAGE);
        assert_eq!(
            (p.pagecache_bytes, p.shmem_bytes, p.kernel_files),
            (3_000_000, 2_000_000, 1000)
        );
        t.set("memory_current_bytes", 1.0).unwrap();
        assert_eq!(
            plan_memory(&t, 0, 1, 1, None, PAGE).kernel_files,
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
    fn windows_at_one_setting_pool_by_instructions() {
        let a = Measured {
            ipc: Some(1.0),
            mpki: Some(4.0),
            dtlb_pki: Some(2.0),
            itlb_pki: None,
            dram: Some(2.0e9),
            accesses_per_s: 100.0,
            instructions_per_s: Some(1.0e9),
            factor: None,
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
            factor: None,
            cores: Some(9.0),
            cv: Some(0.9),
            cv_samples: 39,
        };
        assert_eq!(pool(std::slice::from_ref(&a)), a, "one window is itself");
        let m = pool(&[a.clone(), b.clone()]);
        // Three times the instructions in b: MPKI 4 and 2 pool to 2.5;
        // IPC 1 and 2 over their cycles, 1e9 and 1.5e9, to 1.6.
        assert!((m.mpki.unwrap() - 2.5).abs() < 1e-9, "{m:?}");
        assert!((m.ipc.unwrap() - 1.6).abs() < 1e-9, "{m:?}");
        assert_eq!(m.dram, Some(3.0e9));
        assert_eq!(
            m.dtlb_pki,
            Some(2.0),
            "a side without a value leaves the other"
        );
        assert_eq!(m.itlb_pki, Some(0.5));
        assert_eq!(
            (m.accesses_per_s, m.instructions_per_s),
            (200.0, Some(2.0e9))
        );
        assert_eq!(
            (m.cores, m.cv, m.cv_samples),
            (Some(9.0), Some(0.9), 39),
            "rolling-window fields are the newest window's"
        );
        // Busy windows at the mean rate: four hold 0.4 s of it. Quiet
        // ones, a tenth of the rate, take the busy ones before them.
        let busy = (1.0e9, 0.1);
        let quiet = (1.0e8, 0.1);
        assert_eq!(pool_count(&[busy; 6], 1.0e9, 0.4), 4);
        assert_eq!(
            pool_count(&[busy, busy, quiet, quiet, quiet], 1.0e9, 0.4),
            5
        );
        assert_eq!(pool_count(&[busy; 1], 1.0e9, 0.4), 1, "all there is");
        assert_eq!(pool_count(&[quiet; 3], 1.0e9, 0.4), 3);
        assert_eq!(
            pool_count(&[busy; 6], 0.0, 0.4),
            2,
            "no rate known yet: two"
        );
        assert!((pool_seconds(None) - 0.4).abs() < 1e-12);
        assert!((pool_seconds(Some(1.0)) - 2.4).abs() < 1e-12);
        assert!((pool_seconds(Some(5.0)) - 4.4).abs() < 1e-12, "clamped");
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
        let model = dram_model(&sample, &knobs, 1.0e5, 64).unwrap();
        assert_eq!(
            dram_model(&sample, &knobs, 1.0e5, 128).unwrap(),
            2.0 * model,
            "twice the bytes per line"
        );
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
        assert_eq!(dram_model(&empty, &knobs, 1.0, 64), None);
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
