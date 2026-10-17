//! Fitting knobs to targets: `docs/design.md` section 6. The rules are
//! pure functions of the last window's measurements so they can be
//! driven against a simulated machine in tests.

use crate::loadgen::{Knobs, SLOWEST_CHAINS, clipped_duty_stats};
use crate::profile::Targets;

/// What one window measured, in profile units. Fields the tier cannot
/// measure are `None` and are not fitted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measured {
    /// Busy cores.
    pub cores: Option<f64>,
    /// Burstiness over the window.
    pub cv: Option<f64>,
    /// Instructions per cycle.
    pub ipc: Option<f64>,
    /// L3 misses per thousand instructions.
    pub mpki: Option<f64>,
    /// DRAM bytes per second.
    pub dram: Option<f64>,
    /// dTLB events per thousand instructions.
    pub dtlb_pki: Option<f64>,
    /// iTLB events per thousand instructions.
    pub itlb_pki: Option<f64>,
    /// Generator accesses per second, from the worker counters.
    pub accesses_per_s: f64,
    /// One-second samples behind `cv`.
    pub cv_samples: usize,
    /// Instructions per second, for instructions per access.
    pub instructions_per_s: Option<f64>,
}

impl Measured {
    /// Instructions per generator access, when both are known.
    pub fn instructions_per_access(&self) -> Option<f64> {
        let i = self.instructions_per_s?;
        (self.accesses_per_s > 0.0 && i > 0.0).then(|| i / self.accesses_per_s)
    }
}

/// The targets the fitter drives, in the same units.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Goal {
    /// `cpu_usage_cores`.
    pub cores: Option<f64>,
    /// `cpu_usage_cv_1s`.
    pub cv: Option<f64>,
    /// `ipc`.
    pub ipc: Option<f64>,
    /// `l3_mpki`.
    pub mpki: Option<f64>,
    /// `dram_bytes_per_sec`.
    pub dram: Option<f64>,
    /// `dtlb_pki`.
    pub dtlb_pki: Option<f64>,
    /// `itlb_pki`.
    pub itlb_pki: Option<f64>,
}

impl Goal {
    /// The fitted subset of a target set.
    pub fn from_targets(t: &Targets) -> Goal {
        Goal {
            cores: t.get("cpu_usage_cores"),
            cv: t.get("cpu_usage_cv_1s"),
            ipc: t.get("ipc"),
            mpki: t.get("l3_mpki"),
            dram: t.get("dram_bytes_per_sec"),
            dtlb_pki: t.get("dtlb_pki"),
            itlb_pki: t.get("itlb_pki"),
        }
    }

    /// Whether anything is asked for.
    pub fn is_empty(&self) -> bool {
        [
            self.cores,
            self.cv,
            self.ipc,
            self.mpki,
            self.dram,
            self.dtlb_pki,
            self.itlb_pki,
        ]
        .iter()
        .all(Option::is_none)
    }
}

/// Which dTLB event the profile counts, which decides the sparse page
/// set's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlbRegime {
    /// AMD: L1 misses that hit the L2 TLB, so the set sits between the
    /// two reaches.
    L2Hit,
    /// Intel and ARM: completed walks, so the set exceeds the L2 reach.
    Walk,
}

/// Relative tolerances per kind of target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tolerances {
    /// Ratios: IPC, MPKI, TLB rates, bandwidth.
    pub ratio: f64,
    /// Busy cores.
    pub cores: f64,
    /// Burstiness.
    pub cv: f64,
}

impl Default for Tolerances {
    fn default() -> Self {
        Tolerances {
            ratio: 0.05,
            cores: 0.05,
            cv: 0.05,
        }
    }
}

/// One target after a window.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    /// Profile key.
    pub key: &'static str,
    /// Requested value.
    pub target: f64,
    /// Measured value, if the tier measures it.
    pub measured: Option<f64>,
    /// `(measured - target) / target`.
    pub error: Option<f64>,
    /// Within tolerance.
    pub within: bool,
}

/// Knob ranges the fitter keeps inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Sparse pages available.
    pub sparse_capacity: usize,
    /// Fewest sparse pages the dTLB regime allows: past the L1 reach
    /// under the L2-hit definition, past the L2 reach for walks.
    pub sparse_floor: usize,
    /// Code pages available.
    pub code_capacity: usize,
    /// Pages in the main buffer, for the working-set floor.
    pub main_pages: usize,
    /// Whether there is a page-cache file for page-cache reads.
    pub kreads_available: bool,
}

/// Ops per access. The top, with the visit ceiling, keeps an access
/// near 200k instructions at most, so the duty cycle keeps its clock.
const OPS_RANGE: (f64, f64) = (1.0, 8.0e4);
/// Sparse or code visits per access at most, for the same reason.
const VISITS_CAP: f64 = 1.0e4;
/// IPC target below which non-temporal lines are not used for
/// bandwidth: each line is eight stores retiring at high IPC.
const NT_IPC_FLOOR: f64 = 0.6;
/// Ops move at most this much per pass: the proxy for L3 misses counts
/// fewer of them once compute gives the prefetcher time, so a large
/// step lands far from where the model said.
const OPS_STEP_MAX: f64 = 4.0;
/// One-second samples before busy cores are judged. Each is compared
/// with what the generator intended for that second, so three suffice
/// where thirty did when the long-run mean had to be measured.
pub const MIN_CORES_SAMPLES: usize = 3;
/// One-second samples before burstiness is judged: three factors'
/// worth, since the ratio of the measured to the intended CV needs
/// seconds that intended different things.
pub const MIN_CV_SAMPLES: usize = 3 * crate::loadgen::FACTOR_SECONDS as usize;

/// Whether a target is judged with this many one-second samples in:
/// busy cores and burstiness wait for theirs, the rest never do.
pub fn judged(key: &str, cv_samples: usize) -> bool {
    match key {
        "cpu_usage_cores" => cv_samples >= MIN_CORES_SAMPLES,
        "cpu_usage_cv_1s" => cv_samples >= MIN_CV_SAMPLES,
        _ => true,
    }
}

/// Absolute tolerance per field: a target near zero is met when the
/// measurement is near zero too, whatever the ratio says. Memory gets
/// two hugepages of slack for process-level accounting noise.
pub fn absolute_tolerance(key: &str) -> f64 {
    match key {
        "cpu_usage_cores" | "l3_mpki" => 0.05,
        "cpu_usage_cv_1s" | "ipc" | "dtlb_pki" => 0.02,
        "dram_bytes_per_sec" => 1.0e8,
        "itlb_pki" => 0.01,
        "memory_current_bytes" | "anon_bytes" | "pagecache_bytes" | "shmem_bytes" => {
            8.0 * 1024.0 * 1024.0
        }
        _ => 0.0,
    }
}
const NTLINES_MAX: f64 = 512.0;
const VISITS_RANGE: (f64, f64) = (1.0e-4, 64.0);
/// Smallest working set, in pages, so a shrunken set still spans
/// enough lines to miss in.
const WSET_MIN_PAGES: f64 = 64.0;
/// Seeds for the multiplicative shares, which cannot grow from zero.
const SHARE_SEED: f64 = 0.02;
/// A share below this is given back whole, and the ladder moves on in
/// the same pass instead of spending passes on a knob that no longer
/// matters.
const SHARE_SMALL: f64 = 0.1;
/// Compute iterations per access past which the stream is pure
/// dilution: run length then falls to one line, since every miss it
/// drops saves a thousand instructions of compute.
pub const OPS_DILUTION_CAP: u32 = 4096;
/// MPKI from which, with an IPC target below 1.5, the stall on misses
/// rather than compute sets IPC.
const MEMORY_BOUND_MPKI: f64 = 1.0;
/// Smallest run length: one visit per 1024 accesses.
const LINES_MIN: f64 = 1.0 / 1024.0;
/// The most of the random visits that turn dependent, or back, in one
/// pass.
const DEP_STEP: f64 = 0.2;
/// Page-cache reads per access at most: 256 KiB of copying per access.
const KREADS_MAX: f64 = 64.0;
/// What a page-cache read moves from DRAM: the page, which a file
/// larger than the L3 seldom has cached, as bytes and as lines missed.
const KREAD_BYTES: f64 = 4096.0;
const KREAD_LINES: f64 = 64.0;

/// The fitter: damped multiplicative updates, one pass per window.
#[derive(Clone, Debug, PartialEq)]
pub struct Fitter {
    /// Exponent applied to each correction ratio; 1 is undamped.
    pub damping: f64,
    /// Tolerances.
    pub tol: Tolerances,
    /// Worker threads, fixed for a run.
    pub threads: usize,
    /// Knob ranges.
    pub limits: Limits,
    /// Passes taken so far: windows that moved a knob.
    pub passes: u32,
    /// Last pass's compute work (ops times chains) and instructions per
    /// access, for the secant slope.
    pub last_work_ipa: Option<(f64, f64)>,
    /// Last pass's IPC, to notice when line length stops moving it.
    pub last_ipc: Option<f64>,
    /// Shrinking the working set cost misses once, so the dTLB rule
    /// uses the sequential share from then on.
    pub wset_blocked: bool,
    /// L3 misses come from the core-side DRAM-fill proxy, which does
    /// not count prefetched lines, so a sequential walk and diluted
    /// visits cost counted misses.
    pub proxy_misses: bool,
    /// Last pass's working set, run length and misses per access, to
    /// tell a shrink that cost misses from one that did not.
    pub last_set: Option<(f64, f64, f64)>,
    /// The miss and instruction counts cover kernel mode, so what the
    /// kernel does for a page-cache read is in them.
    pub kernel_counted: bool,
    /// Reversals of the run length at one op per access, where the IPC
    /// and MPKI rules pull it against each other.
    pub lines_track: Reversals,
    /// The same for the sequential share.
    pub seq_track: Reversals,
}

/// A lever's reversals: once it has turned twice within three passes,
/// the two targets pulling it meet nowhere along it and alternating
/// only cycles, so it is frozen for the rest of the fit and the fit
/// holds the compromise.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Reversals {
    last_dir: i8,
    last_pass: u32,
    count: u32,
    /// Whether the lever is frozen.
    pub frozen: bool,
}

impl Reversals {
    /// Note a move of `delta` at `pass`; whether the lever is frozen.
    pub fn observe(&mut self, delta: f64, pass: u32) -> bool {
        if self.frozen || delta == 0.0 {
            return self.frozen;
        }
        let dir = if delta > 0.0 { 1 } else { -1 };
        if self.last_dir != 0 && dir != self.last_dir && pass.saturating_sub(self.last_pass) <= 3 {
            self.count += 1;
            self.frozen = self.count >= 2;
        }
        self.last_dir = dir;
        self.last_pass = pass;
        self.frozen
    }
}

/// Whether the MPKI target is set, measured, and short by more than
/// the tolerance.
fn mpki_short_now(goal: &Goal, m: &Measured, tol: f64) -> bool {
    goal.mpki
        .zip(m.mpki)
        .is_some_and(|(t, v)| v < t * (1.0 - tol))
}

/// A correction ratio raised to the damping, kept to a sane range.
fn factor(target: f64, measured: f64, damping: f64) -> f64 {
    if measured.is_nan() || target.is_nan() || measured <= 0.0 || target <= 0.0 {
        return 1.0;
    }
    (target / measured).powf(damping).clamp(0.25, 4.0)
}

/// What one pass works from: the knobs it starts at, the pooled
/// window, the goal, and the quantities the rules share and hand on.
struct Pass<'a> {
    knobs: &'a Knobs,
    m: &'a Measured,
    goal: &'a Goal,
    /// The damping exponent.
    d: f64,
    st: Vec<Status>,
    /// MPKI within its absolute floor.
    mpki_met: bool,
    /// Whether a prefetched walk and diluted visits may cost counted
    /// misses.
    misses_to_spare: bool,
    ipa: Option<f64>,
    /// Misses per access now.
    mpa_now: Option<f64>,
    /// Compute work, ops times chains, and instructions per unit of it.
    work_now: f64,
    slope: f64,
    /// The share of the misses that followed the run length last pass.
    lines_elasticity: f64,
    /// The compute work the pass wants, as the rules shape it.
    work: f64,
    /// Page-cache reads were given back for MPKI, so the ops rule rests.
    mpki_handled: bool,
    /// Instructions per access and accesses per second after the pass.
    ipa_next: Option<f64>,
    aps_next: f64,
}

impl Pass<'_> {
    fn within(&self, key: &str) -> bool {
        self.st.iter().any(|s| s.key == key && s.within)
    }

    fn mpki_within(&self) -> bool {
        self.mpki_met || self.within("l3_mpki")
    }
}

impl Fitter {
    /// A fitter for `threads` workers with default damping.
    pub fn new(threads: usize, limits: Limits) -> Fitter {
        Fitter {
            damping: 0.5,
            tol: Tolerances::default(),
            threads: threads.max(1),
            limits,
            passes: 0,
            last_work_ipa: None,
            last_ipc: None,
            wset_blocked: false,
            proxy_misses: false,
            last_set: None,
            kernel_counted: false,
            lines_track: Reversals::default(),
            seq_track: Reversals::default(),
        }
    }

    /// Worker count for a target set: the CPU limit if given; otherwise
    /// the busy cores with headroom for burstiness, since the
    /// per-second factor is clipped at full duty and clipping would pull
    /// the mean below the target. At least one, at most `max`.
    pub fn threads_for(t: &Targets, max: usize) -> usize {
        let wanted = match (t.get("cpu_limit_cores"), t.get("cpu_usage_cores")) {
            (Some(limit), _) => limit,
            (None, Some(cores)) => cores * (1.0 + t.get("cpu_usage_cv_1s").unwrap_or(0.0)),
            (None, None) => 1.0,
        };
        (wanted.ceil().max(1.0) as usize).min(max.max(1))
    }

    /// Duty and burstiness knobs for a busy fraction and CV per worker,
    /// from the clipped model: the knobs that give the target mean and
    /// CV once the per-second factor is clipped at full duty. A CV the
    /// duty cannot carry is approached as far as the clip allows.
    pub fn duty_cv_knobs(mean: f64, cv: f64) -> (f64, f64) {
        let mean = mean.clamp(0.0, 1.0);
        if cv <= 0.0 || mean == 0.0 {
            return (mean, 0.0);
        }
        // Nested bisection, the mean first: at a given knob the clipped
        // mean rises with duty, so there is a duty for the mean unless
        // even full duty falls short, in which case the knob clips too
        // much and must fall; among the knobs that keep the mean in
        // reach, the clipped CV rises with the knob. A coupled
        // multiplicative iteration ran away to full duty instead.
        let duty_for = |knob: f64| -> Option<f64> {
            if clipped_duty_stats(1.0, knob).0 < mean - 1e-3 {
                return None;
            }
            let (mut lo, mut hi) = (0.0, 1.0);
            for _ in 0..24 {
                let mid = f64::midpoint(lo, hi);
                if clipped_duty_stats(mid, knob).0 < mean {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            Some(f64::midpoint(lo, hi))
        };
        let (mut lo, mut hi) = (0.0, 6.0);
        for _ in 0..24 {
            let mid = f64::midpoint(lo, hi);
            match duty_for(mid) {
                Some(duty) if clipped_duty_stats(duty, mid).1 < cv => lo = mid,
                _ => hi = mid,
            }
        }
        let knob = lo;
        (duty_for(knob).unwrap_or(1.0), knob)
    }

    /// The step for the dependent share: half the relative IPC error,
    /// between a twentieth and a fifth, so a large error moves fast and
    /// a small one settles instead of cycling.
    fn dep_step(target: f64, measured: f64) -> f64 {
        (0.5 * ((measured - target) / target).abs()).clamp(0.05, DEP_STEP)
    }

    /// How far the distance between two windows' statuses is from
    /// tolerance: the largest relative error over the measured targets,
    /// each divided by its tolerance, so one is the edge of tolerance.
    /// The pass with the smallest distance is the one worth holding.
    pub fn distance(&self, statuses: &[Status]) -> f64 {
        statuses
            .iter()
            .filter_map(|s| {
                let measured = s.measured?;
                let tol = match s.key {
                    "cpu_usage_cores" => self.tol.cores,
                    "cpu_usage_cv_1s" => self.tol.cv,
                    _ => self.tol.ratio,
                };
                let abs = absolute_tolerance(s.key);
                if (measured - s.target).abs() <= abs {
                    return Some(0.0);
                }
                s.error.map(|e| e.abs() / tol)
            })
            .fold(0.0, f64::max)
    }

    /// A first guess from the goal alone: compute from the MPKI target
    /// where it asks for far more than the default, since at eight lines
    /// a visit the instructions per access the misses need are known to
    /// a factor.
    pub fn initial_knobs(&self, goal: &Goal, regime: TlbRegime) -> Knobs {
        let cores = goal.cores.unwrap_or(self.threads as f64);
        let (duty, cv) = Self::duty_cv_knobs(cores / self.threads as f64, goal.cv.unwrap_or(0.0));
        let chains = match goal.ipc {
            Some(i) if i < 0.5 => 0.5,
            Some(i) if i < 1.4 => 1.0,
            Some(i) if i < 2.2 => 2.0,
            Some(_) => 4.0,
            None => 2.0,
        };
        // Eight lines miss about eight times per access over some 270
        // instructions of stream and bookkeeping, and an op is about two
        // and a half more. The estimate replaces the default only where
        // it asks for far more compute, the cache-resident side, where
        // growing ops from the default fourfold a pass cost the most
        // passes; elsewhere the model-based rule is quicker than a guess.
        let ops = match goal.mpki {
            Some(t) if t > 0.0 => {
                let estimate = ((8.2e3 / t - 270.0) / 2.5).clamp(1.0, 4096.0).round() as u32;
                if estimate > 1024 { estimate } else { 64 }
            }
            _ => 64,
        };
        // Walked in a fixed cycle, a set past the TLB's reach misses on
        // every visit, so the set is small and its refill after an idle
        // period costs few misses.
        let sparse_pages = match regime {
            TlbRegime::L2Hit => 256,
            TlbRegime::Walk => 16384,
        }
        .min(self.limits.sparse_capacity)
        .max(1);
        Knobs {
            duty,
            cv,
            ops,
            chains,
            lines: 8.0,
            seq: 0.0,
            dep: 0.0,
            wset: 1.0,
            wfrac: 0.25,
            ntlines: 0.0,
            tlbvisits: if goal.dtlb_pki.is_some_and(|v| v > 0.0) {
                0.2
            } else {
                0.0
            },
            sparse_pages,
            codevisits: if goal.itlb_pki.is_some_and(|v| v > 0.0) {
                0.05
            } else {
                0.0
            },
            code_pages: self.limits.code_capacity.max(1),
            kreads: 0.0,
            period_ms: 10,
        }
    }

    /// Compare a window against the goal, with the relative tolerances
    /// and the absolute floors of [`absolute_tolerance`].
    pub fn statuses(&self, m: &Measured, goal: &Goal) -> Vec<Status> {
        type Row = (&'static str, Option<f64>, Option<f64>, f64);
        let rows: [Row; 7] = [
            ("cpu_usage_cores", goal.cores, m.cores, self.tol.cores),
            ("cpu_usage_cv_1s", goal.cv, m.cv, self.tol.cv),
            ("ipc", goal.ipc, m.ipc, self.tol.ratio),
            ("l3_mpki", goal.mpki, m.mpki, self.tol.ratio),
            ("dram_bytes_per_sec", goal.dram, m.dram, self.tol.ratio),
            ("dtlb_pki", goal.dtlb_pki, m.dtlb_pki, self.tol.ratio),
            ("itlb_pki", goal.itlb_pki, m.itlb_pki, self.tol.ratio),
        ];
        rows.into_iter()
            .filter_map(|(key, target, measured, tol)| {
                let abs_tol = absolute_tolerance(key);
                let target = target?;
                let error = measured.map(|v| {
                    if target > 0.0 {
                        (v - target) / target
                    } else if v == 0.0 {
                        0.0
                    } else {
                        f64::INFINITY
                    }
                });
                let within = measured.is_some_and(|v| (v - target).abs() <= abs_tol)
                    || error.is_some_and(|e| e.abs() <= tol);
                Some(Status {
                    key,
                    target,
                    measured,
                    error,
                    within,
                })
            })
            .collect()
    }

    /// Whether every measured target is within tolerance.
    pub fn converged(statuses: &[Status]) -> bool {
        statuses
            .iter()
            .filter(|s| s.measured.is_some())
            .all(|s| s.within)
    }

    /// Instructions one unit of compute work (one op on one chain)
    /// costs, used until two passes give a measured slope.
    const INSTRUCTIONS_PER_WORK: f64 = 2.5;

    /// One pass. Busy cores, burstiness and IPC move by damped
    /// correction ratios. The ops rule is model-based: instructions per
    /// access are measured, the slope against ops comes from a secant
    /// over the last two passes, and ops is set to the count the MPKI
    /// target needs. The per-access rates are then set from their
    /// measured efficiency against the instructions per access the next
    /// pass will have, so a change in ops never drags them off target.
    pub fn step(&mut self, knobs: &Knobs, m: &Measured, goal: &Goal) -> Knobs {
        let Some(mut p) = self.begin(knobs, m, goal) else {
            return knobs.clone();
        };
        let mut k = knobs.clone();
        Self::cores_and_burstiness(&p, &mut k);
        if self.undo_costly_shrink(&p, &mut k) {
            self.passes += 1;
            return k;
        }
        self.remember(&p);
        self.ipc_ladder(&mut p, &mut k);
        self.dilution(&p, &mut k);
        Self::reads_back(&mut p, &mut k);
        Self::ops_rule(&p, &mut k);
        self.sparse_shrink(&p, &mut k);
        self.misses_at_floor(&p, &mut k);
        // The IPC and MPKI rules are done with the run length and the
        // sequential share; the dTLB rule below may pull them back, and
        // that tug of war is what the reversals count.
        let (lines_mid, seq_mid) = (k.lines, k.seq);
        Self::predict_next(&mut p, &k);
        self.bandwidth(&p, &mut k);
        Self::visit_rates(&p, &mut k);
        self.dtlb_excess(&p, &mut k);
        self.freeze_fought_levers(&p, &mut k, lines_mid, seq_mid);
        k.sparse_pages = k.sparse_pages.clamp(1, self.limits.sparse_capacity.max(1));
        k.code_pages = k.code_pages.clamp(1, self.limits.code_capacity.max(1));
        // A window that moved nothing, because every lever a target
        // needs is at its range, is not a pass: an unreachable target
        // must not spend the budget a later disturbance will need.
        if k != *knobs {
            self.passes += 1;
        }
        k
    }

    /// What a pass works from, or nothing when every judged target is
    /// within tolerance: every rule nudges, a nudge from inside the band
    /// chases noise, and each one is a pass that resets the pooling of
    /// windows. Busy cores and burstiness are judged only once a few
    /// seconds are in.
    fn begin<'a>(&self, knobs: &'a Knobs, m: &'a Measured, goal: &'a Goal) -> Option<Pass<'a>> {
        let st = self.statuses(m, goal);
        if st
            .iter()
            .filter(|s| judged(s.key, m.cv_samples))
            .all(|s| s.within || s.measured.is_none())
        {
            return None;
        }
        // A sequential walk is prefetched, and the L3 miss proxy does not
        // count prefetched lines, so under the proxy the walk costs
        // counted misses, and so does diluting visits. There, both are
        // only for profiles with misses to spare: none asked for, or
        // within the absolute floor. "Not short" was not enough: at
        // target, the first step of the walk took a third of the misses
        // away and the fit unravelled.
        let mpki_met = goal
            .mpki
            .zip(m.mpki)
            .is_some_and(|(t, v)| (v - t).abs() <= absolute_tolerance("l3_mpki"));
        let misses_to_spare = !self.proxy_misses
            || match (goal.mpki, m.mpki) {
                (None, _) => true,
                (Some(t), Some(v)) => (v - t).abs() <= absolute_tolerance("l3_mpki"),
                (Some(_), None) => false,
            };
        // Instructions per access now, and the slope against compute
        // work, which is ops times chains. Below one chain the slow
        // iterations cost about what one-chain iterations do in
        // instructions, so work does not shrink with the knob there.
        let ipa = m.instructions_per_access();
        let work_now = f64::from(knobs.ops) * knobs.chains.max(1.0);
        let mut slope = Self::INSTRUCTIONS_PER_WORK;
        if let (Some(ipa), Some((work_prev, ipa_prev))) = (ipa, self.last_work_ipa) {
            if (work_now - work_prev).abs() >= 1.0 {
                let s = (ipa - ipa_prev) / (work_now - work_prev);
                if s.is_finite() && s > 0.1 * slope && s < 10.0 * slope {
                    slope = s;
                }
            }
        }
        let mpa_now = m.mpki.zip(ipa).map(|(v, ipa)| v / 1000.0 * ipa);
        // How much of the misses followed the run length last pass,
        // between none and all; all until a pass has changed it. Misses
        // the main stream does not make, the sparse set's refills after
        // an idle period, do not follow it, and predicting that they
        // would spiralled ops down while MPKI stood still.
        let lines_elasticity = match (self.last_set, mpa_now) {
            (Some((_, lines_prev, mpa_prev)), Some(mpa))
                if mpa_prev > 0.0
                    && mpa > 0.0
                    && (knobs.lines / lines_prev.max(LINES_MIN)).ln().abs() > 0.1 =>
            {
                ((mpa / mpa_prev).ln() / (knobs.lines / lines_prev.max(LINES_MIN)).ln())
                    .clamp(0.0, 1.0)
            }
            _ => 1.0,
        };
        Some(Pass {
            knobs,
            m,
            goal,
            d: self.damping,
            st,
            mpki_met,
            misses_to_spare,
            ipa,
            mpa_now,
            work_now,
            slope,
            lines_elasticity,
            work: work_now,
            mpki_handled: false,
            ipa_next: None,
            aps_next: m.accesses_per_s,
        })
    }

    /// Busy cores and burstiness start from the clipped model and are
    /// corrected from the long-run values the measured seconds predict,
    /// once there are a few of them.
    fn cores_and_burstiness(p: &Pass, k: &mut Knobs) {
        let (m, goal, d) = (p.m, p.goal, p.d);
        if let (true, Some(t), Some(v), false) = (
            judged("cpu_usage_cores", m.cv_samples),
            goal.cores,
            m.cores,
            p.within("cpu_usage_cores"),
        ) {
            k.duty = (k.duty * factor(t, v, d)).clamp(0.0, 1.0);
        }
        if let (true, Some(t), Some(v), false) = (
            judged("cpu_usage_cv_1s", m.cv_samples),
            goal.cv,
            m.cv,
            p.within("cpu_usage_cv_1s"),
        ) {
            // The knob stays within twice the target so clipping at
            // full duty cannot run it away.
            if v > 0.0 && k.cv > 0.0 {
                k.cv = (k.cv * factor(t, v, d)).clamp(0.0, 2.0 * t);
            }
        }
    }

    /// A shrink of the working set that cost misses, beyond what the
    /// run length explains and above the absolute floor, and left MPKI
    /// short of what shedding every compute instruction would recover,
    /// is undone and not tried again, and the window it spoiled moves
    /// nothing else: the other rules would chase the misses it took
    /// away. Whether it was undone.
    fn undo_costly_shrink(&mut self, p: &Pass, k: &mut Knobs) -> bool {
        let (Some((wset_prev, lines_prev, mpa_prev)), Some(mpa), Some(ipa), Some(t), Some(v)) =
            (self.last_set, p.mpa_now, p.ipa, p.goal.mpki, p.m.mpki)
        else {
            return false;
        };
        let explained = mpa_prev * (p.knobs.lines / lines_prev.max(LINES_MIN));
        // MPKI at one op per access, by the measured slope: the misses
        // stay and the instructions go. At a quarter of stress-ng's set
        // the shrink took three misses in ten per access and a pass later
        // the ops rule had MPKI back at target; undoing it left the dTLB
        // rate twenty times higher.
        let ipa_floor = (ipa - (p.work_now - 1.0) * p.slope).max(1.0);
        let recoverable = v * ipa / ipa_floor;
        if p.knobs.wset < wset_prev
            && !p.mpki_met
            && mpa < 0.75 * explained
            && recoverable < t * (1.0 - self.tol.ratio)
        {
            self.wset_blocked = true;
            k.wset = wset_prev;
            return true;
        }
        false
    }

    /// What the next pass compares itself with.
    fn remember(&mut self, p: &Pass) {
        if let Some(mpa) = p.mpa_now {
            self.last_set = Some((p.knobs.wset, p.knobs.lines, mpa));
        }
        if let Some(ipa) = p.ipa {
            self.last_work_ipa = Some((p.work_now, ipa));
        }
    }

    /// The IPC ladder. Longer runs let the prefetcher stream, raising
    /// IPC at equal MPKI; more chains raise the compute IPC. Run length
    /// is the fine knob; chains move once it is at its range or the
    /// last pass failed to move IPC by the tolerance, which is what a
    /// compute-bound regime looks like. With no MPKI target, compute
    /// work is free to carry IPC too.
    fn ipc_ladder(&mut self, p: &mut Pass, k: &mut Knobs) {
        let (knobs, m, goal, d) = (p.knobs, p.m, p.goal, p.d);
        let (Some(t), Some(v), false) = (goal.ipc, m.ipc, p.within("ipc")) else {
            return;
        };
        let g = factor(t, v, d);
        if goal.mpki.is_none() {
            p.work *= g;
        }
        let stalled = self
            .last_ipc
            .is_some_and(|prev| (v - prev).abs() < prev * self.tol.ratio);
        let chains_can_move = if g > 1.0 {
            k.chains < 4.0
        } else {
            k.chains > SLOWEST_CHAINS
        };
        // With no compute left to cut and MPKI still short, run length
        // is the only lever MPKI has; IPC yields to it there.
        let mpki_short = goal
            .mpki
            .zip(m.mpki)
            .is_some_and(|(t, v)| v < t * (1.0 - self.tol.ratio));
        let lines_locked = g < 1.0 && knobs.ops <= 1 && mpki_short;
        let memory_bound = m.mpki.is_some_and(|x| x >= MEMORY_BOUND_MPKI) && t < 1.5;
        // The shares are given back first: a sequential walk when IPC
        // is high, dependent misses when it is short. A small share
        // goes back whole and the ladder continues this pass.
        let mut handled = false;
        if g < 1.0 && k.seq > 0.0 {
            handled = k.seq >= SHARE_SMALL;
            k.seq = if handled { k.seq * g } else { 0.0 };
        } else if g > 1.0 && k.dep > 0.0 {
            // Given back in proportion to the shortfall, between a
            // twentieth and a fifth a pass: a fixed fifth each way made
            // a limit cycle around a target that needed a tenth.
            handled = k.dep >= SHARE_SMALL;
            k.dep = if handled {
                (k.dep - Self::dep_step(t, v)).max(0.0)
            } else {
                0.0
            };
        }
        // A visit share is not a run: a run of one on every access
        // would undo the dilution the share is, so IPC short leaves
        // it to the chains.
        let lines_can_move = if g > 1.0 {
            (1.0..64.0).contains(&k.lines)
        } else {
            k.lines > 1.0 && !lines_locked
        };
        let dep_up = |k: &mut Knobs| k.dep = (k.dep + Self::dep_step(t, v)).min(1.0);
        let chains_move = |k: &mut Knobs| k.chains = (k.chains * g).clamp(SLOWEST_CHAINS, 4.0);
        let lines_move = |k: &mut Knobs| k.lines = (k.lines * g).clamp(1.0, 64.0);
        if !handled && g < 1.0 && memory_bound {
            // Stalls set IPC here: serialize the misses once compute is
            // down to a chain or has stopped helping; then fewer lines
            // per visit, which is less overlap, undamped since the
            // ops rule keeps MPKI; then compute.
            if k.dep < 1.0 && (k.chains <= 1.0 || stalled || knobs.ops <= 1) {
                dep_up(k);
            } else if lines_can_move {
                k.lines = (k.lines * factor(t, v, 1.0)).max(1.0);
            } else if chains_can_move {
                chains_move(k);
            }
        } else if !handled && g < 1.0 {
            // Compute sets IPC: shorter runs, then fewer chains. With
            // both at their floors and a visit on only a share of
            // accesses, the access is scaled up whole, compute and
            // visits alike, so the slow chain carries more of the
            // instructions at unchanged misses per instruction.
            if lines_can_move && !stalled {
                lines_move(k);
            } else if chains_can_move {
                chains_move(k);
            } else if k.lines <= 1.0 && goal.mpki.is_some() {
                let up = (1.0 / g).powi(2).min(2.0);
                p.work *= up;
                k.lines = (k.lines * up).min(64.0);
            } else {
                dep_up(k);
            }
        } else if !handled {
            // IPC short: longer runs, more chains, then a sequential
            // walk that lets the prefetcher stream; when none of the
            // others can move, run length keeps moving even if it
            // seemed to have stalled.
            let others_can_move = chains_can_move || (k.seq < 1.0 && p.misses_to_spare);
            if lines_can_move && (!stalled || !others_can_move) {
                lines_move(k);
            } else if chains_can_move {
                chains_move(k);
            } else if k.seq < 1.0 && p.misses_to_spare {
                k.seq = (k.seq.max(SHARE_SEED) * g).clamp(0.0, 1.0);
            }
        }
        self.last_ipc = Some(v);
    }

    /// Past the dilution cap, with misses not short, the stream is pure
    /// dilution and the run length halves each pass, since every miss
    /// dropped saves a thousand instructions of compute.
    fn dilution(&self, p: &Pass, k: &mut Knobs) {
        if p.knobs.ops > OPS_DILUTION_CAP
            && k.lines > LINES_MIN
            && !mpki_short_now(p.goal, p.m, self.tol.ratio)
        {
            k.lines = (k.lines * 0.5).max(LINES_MIN);
        }
    }

    /// Page-cache reads give misses back before compute dilutes them,
    /// since they were the last lever taken: by the model, the misses
    /// over per access over the lines a page holds, damped.
    fn reads_back(p: &mut Pass, k: &mut Knobs) {
        if let (Some(t), Some(v), Some(ipa)) = (p.goal.mpki, p.m.mpki, p.ipa) {
            if k.kreads > 0.0 && !p.mpki_within() && v > t {
                let over = (v - t) / 1000.0 * ipa;
                k.kreads = (k.kreads - p.d * over / KREAD_LINES).max(0.0);
                p.mpki_handled = true;
            }
        }
    }

    /// The ops rule, model-based: misses per access follow the run
    /// length the IPC rule just chose; at that length, the instructions
    /// per access the target needs are known, and the work that gives
    /// them. Within the absolute floor the target is met and what
    /// remains is mostly noise; chasing it swung ops between one and
    /// tens of thousands.
    fn ops_rule(p: &Pass, k: &mut Knobs) {
        let (knobs, m, goal, d) = (p.knobs, p.m, p.goal, p.d);
        let mut work = p.work;
        let quiet = p.mpki_within() || p.mpki_handled;
        match (goal.mpki, m.mpki, p.ipa) {
            (Some(t), Some(v), Some(ipa)) if t > 0.0 && v > 0.0 && !quiet => {
                let misses_per_access = v / 1000.0
                    * ipa
                    * (k.lines / knobs.lines.max(LINES_MIN)).powf(p.lines_elasticity);
                let want_ipa = misses_per_access * 1000.0 / t;
                work += d * (want_ipa - ipa) / p.slope;
            }
            (Some(t), Some(v), None) if !quiet => work *= factor(v, t, d),
            _ => {}
        }
        let work = work.clamp(p.work_now / OPS_STEP_MAX, p.work_now * OPS_STEP_MAX);
        k.ops = (work / k.chains.max(1.0))
            .clamp(OPS_RANGE.0, OPS_RANGE.1)
            .round() as u32;
    }

    /// With compute at its ceiling, the main stream diluted to its
    /// floor and misses still high, the misses left are the sparse
    /// set's own: its lines, one per page, and their page-table
    /// entries, refetched after every idle period of the duty cycle.
    /// Half the set per pass, down to the floor the dTLB regime needs;
    /// the visit rate makes up for the hit rate. Not before the main
    /// stream is diluted away: halving both at once took the set to
    /// the floor while the main stream still made the misses, and at
    /// the floor few visits miss the L1, so the dTLB rate ran out of
    /// visits.
    fn sparse_shrink(&self, p: &Pass, k: &mut Knobs) {
        if let (Some(t), Some(v)) = (p.goal.mpki, p.m.mpki) {
            if k.ops >= OPS_RANGE.1 as u32
                && p.knobs.ops >= OPS_RANGE.1 as u32
                && k.lines <= LINES_MIN
                && !p.mpki_within()
                && v > t
                && k.sparse_pages > self.limits.sparse_floor
            {
                k.sparse_pages = (k.sparse_pages / 2).max(self.limits.sparse_floor);
            }
        }
    }

    /// Compute cannot go below one op per access; past that, more lines
    /// per access are the only way to more misses, undamped since misses
    /// follow run length about linearly and passes are few. With the run
    /// at its range and the kernel's misses in the count: page-cache
    /// reads, each a page the kernel copies out of the page cache, which
    /// is where a page-cache reader's kernel-mode misses come from. By
    /// the model, the misses short per access over the lines a page
    /// holds, damped: a read is sixty-four misses, and a whole one per
    /// access swung MPKI by thirty.
    fn misses_at_floor(&self, p: &Pass, k: &mut Knobs) {
        let (Some(t), Some(v)) = (p.goal.mpki, p.m.mpki) else {
            return;
        };
        if k.ops <= 1 && p.knobs.ops <= 1 && v < t * (1.0 - self.tol.ratio) {
            k.lines = (k.lines * factor(t, v, 1.0)).clamp(LINES_MIN, 64.0);
            if let (true, true, true, Some(ipa)) = (
                self.kernel_counted,
                self.limits.kreads_available,
                p.knobs.lines >= 64.0,
                p.ipa,
            ) {
                let short = (t - v) / 1000.0 * ipa;
                k.kreads = (k.kreads + p.d * short / KREAD_LINES).clamp(0.0, KREADS_MAX);
            }
        }
    }

    /// Instructions per access after this pass, by the same model, and
    /// the access rate it implies at unchanged cycles per instruction.
    fn predict_next(p: &mut Pass, k: &Knobs) {
        p.ipa_next = p
            .ipa
            .map(|i| (i + p.slope * (f64::from(k.ops) * k.chains.max(1.0) - p.work_now)).max(1.0));
        p.aps_next = match (p.ipa, p.ipa_next) {
            (Some(i), Some(n)) if p.m.accesses_per_s > 0.0 => p.m.accesses_per_s * i / n,
            _ => p.m.accesses_per_s,
        };
    }

    /// Bandwidth comes last and only while IPC and MPKI are near their
    /// targets, within twice the tolerance, since both its levers
    /// change them: stores to missed lines write back, and non-temporal
    /// lines bypass the caches. The write share goes first, up to every
    /// line, then non-temporal lines by the model's count, damped
    /// linearly since it is an absolute estimate, then page-cache reads
    /// where non-temporal lines cannot carry it.
    fn bandwidth(&self, p: &Pass, k: &mut Knobs) {
        let (m, goal, d) = (p.m, p.goal, p.d);
        let ratios_within =
            p.st.iter()
                .filter(|s| s.key == "ipc" || s.key == "l3_mpki")
                .all(|s| s.within || s.error.is_none_or(|e| e.abs() <= 2.0 * self.tol.ratio));
        // Measured on Zen 4 at fixed knobs: the write share from a quarter
        // to all lines left IPC and MPKI where they were and raised
        // bandwidth by half; 64 non-temporal lines per access quadrupled
        // IPC and divided MPKI by ten. Non-temporal lines are therefore
        // only for profiles whose IPC can carry them.
        let nt_fits = goal.ipc.is_none_or(|t| t >= NT_IPC_FLOOR);
        let (Some(t), Some(v)) = (goal.dram, m.dram) else {
            return;
        };
        if !(p.aps_next > 0.0 && ratios_within) {
            return;
        }
        if v < t * (1.0 - self.tol.ratio) && k.wfrac < 1.0 {
            k.wfrac = (k.wfrac * factor(t, v, d)).min(1.0);
        } else if v > t * (1.0 + self.tol.ratio) && k.ntlines <= 0.0 && k.wfrac > 0.0 {
            k.wfrac = (k.wfrac * factor(t, v, d)).max(0.0);
        } else if v < t * (1.0 - self.tol.ratio)
            && (!nt_fits || k.ntlines >= NTLINES_MAX)
            && self.limits.kreads_available
        {
            // Non-temporal lines cannot carry it: page-cache reads,
            // a page from DRAM each, by the shortfall, damped.
            let need = (t - v) / (KREAD_BYTES * p.aps_next);
            k.kreads = (k.kreads + d * need).clamp(0.0, KREADS_MAX);
        } else if v < t * (1.0 - self.tol.ratio) && !nt_fits {
            // Bandwidth waits: the lever left would take IPC with it.
        } else {
            let per_line_now = 64.0 * m.accesses_per_s;
            let nt_now = k.ntlines * per_line_now;
            let other_per_access = (v - nt_now).max(0.0) / m.accesses_per_s.max(1e-9);
            let need_lines = ((t / p.aps_next - other_per_access) / 64.0).max(0.0);
            let now = k.ntlines;
            k.ntlines = (now + d * (need_lines - now)).clamp(0.0, NTLINES_MAX);
        }
    }

    /// The per-access rates of the sparse and code streams: full
    /// compensation for the change in instructions per access, which
    /// the model predicts, then a damped correction of the measured
    /// error. Visits per access may grow with the access itself: a
    /// visit is a few instructions, so a quarter of them is the ceiling.
    fn visit_rates(p: &Pass, k: &mut Knobs) {
        let (m, goal, d) = (p.m, p.goal, p.d);
        let visits_max = VISITS_RANGE
            .1
            .max((p.ipa_next.unwrap_or(0.0) / 4.0).min(VISITS_CAP));
        let rate_rule = |target: Option<f64>, measured: Option<f64>, visits: f64, met: bool| {
            let (Some(t), Some(v)) = (target, measured) else {
                return visits;
            };
            if t <= 0.0 {
                return 0.0;
            }
            let correction = if met { 1.0 } else { factor(t, v, d) };
            match (p.ipa, p.ipa_next) {
                (Some(ipa), Some(next)) if visits > 0.0 && v > 0.0 => {
                    (visits * (next / ipa) * correction).clamp(VISITS_RANGE.0, visits_max)
                }
                _ if visits > 0.0 => (visits * correction).clamp(VISITS_RANGE.0, visits_max),
                _ => VISITS_RANGE.0 * 100.0,
            }
        };
        k.tlbvisits = rate_rule(goal.dtlb_pki, m.dtlb_pki, k.tlbvisits, p.within("dtlb_pki"));
        k.codevisits = rate_rule(
            goal.itlb_pki,
            m.itlb_pki,
            k.codevisits,
            p.within("itlb_pki"),
        );
    }

    /// The main stream's own pages cause dTLB events the sparse stream
    /// cannot take back. When they alone exceed the target, touch fewer
    /// pages, or walk them in order when the set cannot shrink or IPC is
    /// short and would gain from streaming; otherwise dTLB waits, since
    /// every lever left would cost misses the profile cannot spare.
    fn dtlb_excess(&self, p: &Pass, k: &mut Knobs) {
        let (knobs, m, goal, d) = (p.knobs, p.m, p.goal, p.d);
        let (Some(t), Some(v), Some(ipa)) = (goal.dtlb_pki, m.dtlb_pki, p.ipa) else {
            return;
        };
        let sparse_at_most = 1000.0 * knobs.tlbvisits / ipa;
        let excess = v - sparse_at_most;
        let abs_tol = absolute_tolerance("dtlb_pki");
        if excess <= t * (1.0 + self.tol.ratio) + abs_tol {
            return;
        }
        // Half the set per pass at most: the miss rate is steep at
        // the cache edge, and the guard above only sees one step.
        let f = factor(t.max(0.5 * abs_tol), excess, d).max(0.5);
        let wset_min = (WSET_MIN_PAGES / self.limits.main_pages.max(1) as f64).min(1.0);
        let ipc_short = goal
            .ipc
            .zip(m.ipc)
            .is_some_and(|(t, v)| v < t * (1.0 - self.tol.ratio));
        let ipc_high = goal
            .ipc
            .zip(m.ipc)
            .is_some_and(|(t, v)| v > t * (1.0 + self.tol.ratio));
        if k.wset > wset_min && !self.wset_blocked && !ipc_short {
            k.wset = (k.wset * f).clamp(wset_min, 1.0);
        } else if p.misses_to_spare && !ipc_high {
            k.seq = (k.seq.max(SHARE_SEED) / f).clamp(0.0, 1.0);
        } else if p.misses_to_spare {
            // A walk would raise IPC further; fewer visits per
            // access dilute the events instead.
            k.lines = (k.lines * f).max(LINES_MIN);
        }
    }

    /// At one op per access the IPC and MPKI rules pull the run length
    /// and the sequential share against each other: a prefetched walk
    /// or a longer run brings the misses the one wants and the IPC the
    /// other does not. A lever that has turned twice within three
    /// passes is frozen where it stands, since the two targets meet
    /// nowhere along it; the run then adopts the closest pass.
    fn freeze_fought_levers(&mut self, p: &Pass, k: &mut Knobs, lines_mid: f64, seq_mid: f64) {
        if p.knobs.ops > 1 {
            return;
        }
        let pass = self.passes + 1;
        self.lines_track.observe(lines_mid - p.knobs.lines, pass);
        if self.lines_track.observe(k.lines - lines_mid, pass) {
            k.lines = p.knobs.lines;
        }
        self.seq_track.observe(seq_mid - p.knobs.seq, pass);
        if self.seq_track.observe(k.seq - seq_mid, pass) {
            k.seq = p.knobs.seq;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loadgen::Buffers;
    use proptest::prelude::*;

    const LIMITS: Limits = Limits {
        sparse_capacity: 16384,
        sparse_floor: 128,
        code_capacity: 16384,
        main_pages: 262_144,
        kreads_available: true,
    };

    /// A stand-in machine: monotone in every knob the way the real one
    /// is expected to be, with made-up constants.
    fn plant(k: &Knobs, threads: usize) -> Measured {
        let chains_eff = if k.chains < 1.0 {
            0.15 + 0.85 * (k.chains - 0.25) / 0.75
        } else if k.chains <= 2.0 {
            1.0 + 0.6 * (k.chains - 1.0)
        } else {
            1.6 + 0.6 * (k.chains - 2.0)
        };
        let ops = f64::from(k.ops);
        let lines = k.lines;
        let nt = k.ntlines;
        let instr = 2.0 * ops * k.chains.max(1.0)
            + 3.0 * lines
            + 4.0 * nt
            + 2.0 * k.tlbvisits
            + 3.0 * k.codevisits
            + 2500.0 * k.kreads
            + 12.0;
        // The set: a 1 GiB buffer; misses need more than 32 MiB of it,
        // first-level TLB events more than 144 MiB (hugepage reach).
        let pages = k.wset * LIMITS.main_pages as f64;
        let miss_rate = if pages > 8192.0 { 1.0 } else { 0.02 };
        let tlb_rate = if pages > 36_864.0 { 1.0 } else { 0.05 };
        let misses = if lines >= 1.0 {
            0.95 * lines + 0.6
        } else {
            1.55 * lines
        } * miss_rate;
        // Overlapped misses cost a fraction of their latency and the
        // prefetcher hides more of a longer run; dependent visits pay the
        // whole latency per line; a sequential walk is mostly prefetched.
        let overlap = (1.0 - k.dep) * 0.3 / (1.0 + 0.3 * lines) + k.dep;
        // A page-cache read is a kernel copy of 64 lines, streamed.
        let kread_misses = 58.0 * k.kreads;
        let stall = misses * 250.0 * overlap * (1.0 - 0.6 * k.seq) + kread_misses * 20.0;
        let main_tlb = ((1.0 - k.seq) + k.seq * lines / 64.0) * tlb_rate * lines.min(1.0);
        let cycles = ops * 4.0 / chains_eff + stall + 10.0;
        let duty = k.duty;
        let aps = threads as f64 * duty * 3.0e9 / cycles;
        let hit = if k.sparse_pages < 96 {
            0.05
        } else if k.sparse_pages <= 3072 {
            0.98
        } else {
            0.1
        };
        let walk = if k.code_pages > 1024 { 0.9 } else { 0.05 };
        Measured {
            cores: Some(threads as f64 * duty * 1.02),
            cv: Some(k.cv * if duty > 0.9 { 0.7 } else { 1.0 }),
            ipc: Some(instr / cycles),
            mpki: Some(1000.0 * (misses + kread_misses) / instr),
            dram: Some(
                (misses * 64.0 + nt * 64.0 + k.wfrac * lines * 32.0 + k.kreads * 4096.0) * aps,
            ),
            dtlb_pki: Some(1000.0 * (k.tlbvisits * hit + main_tlb) / instr),
            itlb_pki: Some(1000.0 * k.codevisits * walk / instr),
            accesses_per_s: aps,
            cv_samples: 300,
            instructions_per_s: Some(instr * aps),
        }
    }

    fn goal_of(m: &Measured) -> Goal {
        Goal {
            cores: m.cores,
            cv: m.cv,
            ipc: m.ipc,
            mpki: m.mpki,
            dram: m.dram,
            dtlb_pki: m.dtlb_pki,
            itlb_pki: m.itlb_pki,
        }
    }

    fn converge(
        goal: &Goal,
        threads: usize,
        regime: TlbRegime,
        max_pass: u32,
    ) -> (Fitter, Knobs, Vec<Status>) {
        let mut f = Fitter::new(threads, LIMITS);
        let mut k = f.initial_knobs(goal, regime);
        let mut statuses = Vec::new();
        for _ in 0..max_pass {
            let m = plant(&k, threads);
            statuses = f.statuses(&m, goal);
            if Fitter::converged(&statuses) {
                break;
            }
            k = f.step(&k, &m, goal);
        }
        (f, k, statuses)
    }

    #[test]
    fn feasible_goals_converge_from_the_initial_guess() {
        let cases = [
            (
                "memory bound",
                Knobs {
                    duty: 0.6,
                    cv: 0.3,
                    ops: 40,
                    chains: 1.0,
                    lines: 4.0,
                    wfrac: 0.75,
                    ntlines: 0.0,
                    tlbvisits: 0.5,
                    sparse_pages: 2048,
                    codevisits: 0.02,
                    code_pages: 16384,
                    ..Knobs::default()
                },
                4,
                TlbRegime::L2Hit,
            ),
            (
                "compute bound",
                Knobs {
                    duty: 0.9,
                    cv: 0.1,
                    ops: 400,
                    chains: 4.0,
                    lines: 12.0,
                    ntlines: 0.0,
                    tlbvisits: 0.05,
                    sparse_pages: 16384,
                    codevisits: 0.3,
                    code_pages: 16384,
                    ..Knobs::default()
                },
                8,
                TlbRegime::Walk,
            ),
            (
                "bursty",
                Knobs {
                    duty: 0.3,
                    cv: 0.9,
                    ops: 120,
                    chains: 2.0,
                    lines: 16.0,
                    ntlines: 8.0,
                    tlbvisits: 2.0,
                    sparse_pages: 2048,
                    codevisits: 1.5,
                    code_pages: 16384,
                    ..Knobs::default()
                },
                2,
                TlbRegime::L2Hit,
            ),
        ];
        for (name, truth, threads, regime) in cases {
            let goal = goal_of(&plant(&truth, threads));
            let (f, k, statuses) = converge(&goal, threads, regime, 40);
            assert!(
                Fitter::converged(&statuses),
                "{name}: {statuses:#?}\nknobs {k:#?}"
            );
            assert!(f.passes <= 30, "{name}: {} passes", f.passes);
            assert_eq!(statuses.len(), 7);
        }
    }

    #[test]
    fn stream_shapes_are_found_through_the_main_stream_knobs() {
        // Each truth needs one of the stream knobs: serialized misses for
        // a latency-bound target, a sequential walk for a streaming one,
        // a small set for random access over hugepages with no TLB events.
        type Shape = fn(&Knobs) -> bool;
        let cases: [(&str, Knobs, Shape); 3] = [
            (
                "latency bound",
                Knobs {
                    duty: 1.0,
                    ops: 1,
                    chains: 0.25,
                    lines: 2.0,
                    dep: 0.7,
                    tlbvisits: 0.0,
                    sparse_pages: 2048,
                    code_pages: 16384,
                    ..Knobs::default()
                },
                |k: &Knobs| k.dep > 0.3,
            ),
            (
                "streaming",
                Knobs {
                    duty: 1.0,
                    ops: 300,
                    chains: 4.0,
                    lines: 64.0,
                    seq: 1.0,
                    tlbvisits: 0.0,
                    sparse_pages: 2048,
                    code_pages: 16384,
                    ..Knobs::default()
                },
                |k: &Knobs| k.seq > 0.5,
            ),
            (
                "hugepage random",
                Knobs {
                    duty: 1.0,
                    ops: 20,
                    chains: 1.0,
                    lines: 2.0,
                    wset: 0.1,
                    tlbvisits: 0.0,
                    sparse_pages: 2048,
                    code_pages: 16384,
                    ..Knobs::default()
                },
                |k: &Knobs| k.wset < 0.15 && k.wset > 0.03,
            ),
        ];
        for (name, truth, shape) in cases {
            let goal = goal_of(&plant(&truth, 4));
            let (f, k, statuses) = converge(&goal, 4, TlbRegime::L2Hit, 40);
            // Bandwidth follows from the misses and the run length the
            // ratios chose, and the write share trims it by a few percent
            // at most; the ratios and the shape are what this checks.
            let ratios: Vec<Status> = statuses
                .iter()
                .filter(|s| s.key != "dram_bytes_per_sec")
                .cloned()
                .collect();
            assert!(
                Fitter::converged(&ratios),
                "{name}: {statuses:#?}\nknobs {k:#?}"
            );
            let dram = statuses
                .iter()
                .find(|s| s.key == "dram_bytes_per_sec")
                .unwrap();
            assert!(
                dram.error.unwrap().abs() < 0.25,
                "{name}: bandwidth directional at least: {dram:?}"
            );
            assert!(shape(&k), "{name}: knobs {k:#?} after {} passes", f.passes);
        }
    }

    #[test]
    fn under_the_proxy_a_walk_waits_for_misses_to_spare() {
        let goal = Goal {
            ipc: Some(0.22),
            mpki: Some(20.0),
            dtlb_pki: Some(0.003),
            ..Goal::default()
        };
        let k = Knobs {
            ops: 1,
            chains: 0.5,
            lines: 9.0,
            dep: 0.5,
            wset: 0.125,
            ..Knobs::default()
        };
        let m = Measured {
            ipc: Some(0.20),
            mpki: Some(20.1),
            dtlb_pki: Some(0.28),
            accesses_per_s: 1.0e7,
            instructions_per_s: Some(3.8e9),
            ..Measured::default()
        };
        let mut proxy = Fitter::new(4, LIMITS);
        proxy.proxy_misses = true;
        proxy.wset_blocked = true;
        let next = proxy.step(&k, &m, &goal);
        assert_eq!(next.seq, 0.0, "{next:?}");
        assert!(next.lines >= 9.0, "{next:?}");
        let mut counted = Fitter::new(4, LIMITS);
        counted.wset_blocked = true;
        let next = counted.step(&k, &m, &goal);
        assert!(
            next.seq > 0.0,
            "with counted misses the walk is allowed: {next:?}"
        );
    }

    #[test]
    fn a_lever_that_turns_twice_at_one_op_is_frozen() {
        let mut r = Reversals::default();
        assert!(!r.observe(1.0, 1));
        assert!(!r.observe(1.0, 2), "same way: no reversal");
        assert!(!r.observe(-1.0, 3), "one reversal");
        assert!(r.observe(1.0, 4), "two: frozen");
        assert!(r.observe(0.0, 9), "stays frozen");
        let mut slow = Reversals::default();
        assert!(!slow.observe(1.0, 1));
        assert!(!slow.observe(-1.0, 8), "far apart: not a cycle");
        assert!(!slow.observe(1.0, 15));
        assert!(!slow.frozen);

        // At one op with counted misses the run length is raised by the
        // MPKI rule and halved by the dTLB rule in turn; after the second
        // reversal it stays put.
        let goal = Goal {
            ipc: Some(0.22),
            mpki: Some(95.0),
            dtlb_pki: Some(0.01),
            ..Goal::default()
        };
        let mut f = Fitter::new(4, LIMITS);
        f.wset_blocked = true;
        let mut k = Knobs {
            ops: 1,
            chains: 0.5,
            lines: 20.0,
            dep: 1.0,
            wset: 0.125,
            tlbvisits: 0.0001,
            ..Knobs::default()
        };
        let short = Measured {
            ipc: Some(0.19),
            mpki: Some(50.0),
            dtlb_pki: Some(0.005),
            accesses_per_s: 1.0e7,
            instructions_per_s: Some(5.0e9),
            ..Measured::default()
        };
        let walks = Measured {
            ipc: Some(0.30),
            mpki: Some(60.0),
            dtlb_pki: Some(2.0),
            ..short.clone()
        };
        let mut moves = Vec::new();
        for m in [&short, &walks, &short, &walks, &short, &walks] {
            let next = f.step(&k, m, &goal);
            moves.push(next.lines - k.lines);
            k = next;
        }
        // The second turn freezes the lever and cancels its own move.
        assert!(moves[0] > 0.0 && moves[1] < 0.0, "{moves:?}");
        assert!(f.lines_track.frozen, "{moves:?}");
        assert_eq!(moves[2..], [0.0, 0.0, 0.0, 0.0], "{moves:?}");
    }

    #[test]
    fn page_cache_reads_fill_the_misses_the_kernel_counts() {
        let goal = Goal {
            ipc: Some(1.4),
            mpki: Some(2.4),
            ..Goal::default()
        };
        let k = Knobs {
            ops: 1,
            lines: 64.0,
            chains: 1.0,
            ..Knobs::default()
        };
        // 2000 instructions per access, one miss per thousand short.
        let short = Measured {
            ipc: Some(1.4),
            mpki: Some(1.4),
            accesses_per_s: 1.0e4,
            instructions_per_s: Some(2.0e7),
            ..Measured::default()
        };
        let mut f = Fitter::new(1, LIMITS);
        f.kernel_counted = true;
        let next = f.step(&k, &short, &goal);
        let want = 0.5 * (1.0 / 1000.0 * 2000.0) / KREAD_LINES;
        assert!((next.kreads - want).abs() < 1e-9, "{next:?}");
        assert_eq!(next.ops, 1, "compute is at its floor already");
        // Misses over: the reads give back first, compute stays.
        let over = Measured {
            mpki: Some(4.0),
            ..short.clone()
        };
        let had = Knobs {
            kreads: 0.05,
            ..k.clone()
        };
        let next = f.step(&had, &over, &goal);
        assert!(next.kreads < 0.05 && next.kreads > 0.0, "{next:?}");
        assert_eq!(next.ops, 1, "{next:?}");
        // Without the kernel in the count the reads are invisible and
        // not used.
        let mut user_only = Fitter::new(1, LIMITS);
        let next = user_only.step(&k, &short, &goal);
        assert_eq!(next.kreads, 0.0, "{next:?}");
    }

    #[test]
    fn bandwidth_takes_page_cache_reads_where_non_temporal_lines_cannot() {
        let goal = Goal {
            ipc: Some(0.3),
            mpki: Some(5.0),
            dram: Some(8.0e8),
            ..Goal::default()
        };
        let k = Knobs {
            ops: 4,
            lines: 8.0,
            wfrac: 1.0,
            ..Knobs::default()
        };
        let m = Measured {
            ipc: Some(0.3),
            mpki: Some(5.0),
            dram: Some(1.0e7),
            accesses_per_s: 1.0e5,
            instructions_per_s: Some(1.0e8),
            ..Measured::default()
        };
        let mut f = Fitter::new(1, LIMITS);
        let next = f.step(&k, &m, &goal);
        assert_eq!(next.ntlines, 0.0, "an IPC of 0.3 cannot carry them");
        assert!(next.kreads > 0.0, "{next:?}");
        let mut without = Fitter::new(
            1,
            Limits {
                kreads_available: false,
                ..LIMITS
            },
        );
        let next = without.step(&k, &m, &goal);
        assert_eq!((next.kreads, next.ntlines), (0.0, 0.0), "{next:?}");
    }

    #[test]
    fn a_shrink_that_costs_misses_is_undone_and_blocked() {
        let mut f = Fitter::new(4, LIMITS);
        f.proxy_misses = true;
        let goal = Goal {
            mpki: Some(10.0),
            dtlb_pki: Some(0.0),
            ..Goal::default()
        };
        let measured = |mpki: f64| Measured {
            mpki: Some(mpki),
            dtlb_pki: Some(2.0),
            accesses_per_s: 1.0e6,
            instructions_per_s: Some(1.0e8),
            ..Measured::default()
        };
        // Pass one at the full set: dTLB excess shrinks it.
        let k0 = Knobs {
            ops: 1,
            ..Knobs::default()
        };
        let k1 = f.step(&k0, &measured(10.0), &goal);
        assert!(k1.wset < 1.0 && !f.wset_blocked, "{k1:?}");
        // Misses fell by far more than the unchanged run length explains.
        let k2 = f.step(&k1, &measured(1.0), &goal);
        assert_eq!(k2.wset, 1.0, "undone");
        assert!(f.wset_blocked);
        assert_eq!(
            Knobs {
                wset: k1.wset,
                ..k2.clone()
            },
            k1,
            "the spoiled window moves nothing else"
        );
        let k3 = f.step(&k2, &measured(8.0), &goal);
        assert_eq!(k3.wset, 1.0, "blocked");
        assert_eq!(
            k3.seq, 0.0,
            "misses short: the walk would cost them, so dTLB waits"
        );
        // A shrink that keeps its misses stands.
        let mut g = Fitter::new(4, LIMITS);
        let k1 = g.step(&k0, &measured(10.0), &goal);
        let k2 = g.step(&k1, &measured(9.5), &goal);
        assert!(k2.wset < k1.wset && !g.wset_blocked, "{k2:?}");
    }

    #[test]
    fn high_ipc_moves_on_from_a_small_share_and_serializes_when_memory_bound() {
        // Compute-bound, IPC high, a leftover sequential share: it goes
        // back whole and the chains still move in the same pass.
        let mut f = Fitter::new(8, LIMITS);
        let goal = Goal {
            ipc: Some(3.7),
            mpki: Some(0.002),
            ..Goal::default()
        };
        let k = Knobs {
            ops: 80_000,
            chains: 4.0,
            lines: 9.0,
            seq: 0.03,
            ..Knobs::default()
        };
        let m = Measured {
            ipc: Some(4.45),
            mpki: Some(0.003),
            accesses_per_s: 1.0e4,
            instructions_per_s: Some(1.7e11),
            ..Measured::default()
        };
        let next = f.step(&k, &m, &goal);
        assert_eq!(next.seq, 0.0);
        assert!(next.lines < k.lines || next.chains < k.chains, "{next:?}");
        assert!(
            next.lines <= 0.5 * k.lines,
            "pure dilution halves the run length: {next:?}"
        );
        // Memory-bound, IPC high, compute already down to a chain:
        // dependent misses, not more passes on chains.
        let mut f = Fitter::new(4, LIMITS);
        let goal = Goal {
            ipc: Some(0.7),
            mpki: Some(6.2),
            ..Goal::default()
        };
        let k = Knobs {
            ops: 150,
            chains: 0.85,
            lines: 5.9,
            ..Knobs::default()
        };
        let m = Measured {
            ipc: Some(1.0),
            mpki: Some(6.4),
            accesses_per_s: 1.0e6,
            instructions_per_s: Some(1.6e10),
            ..Measured::default()
        };
        let next = f.step(&k, &m, &goal);
        assert!(next.dep > 0.0, "{next:?}");
        // IPC high and dTLB in excess with the set blocked: no walk.
        let mut f = Fitter::new(4, LIMITS);
        f.wset_blocked = true;
        let goal = Goal {
            ipc: Some(0.22),
            dtlb_pki: Some(0.003),
            ..Goal::default()
        };
        let m = Measured {
            ipc: Some(1.0),
            dtlb_pki: Some(0.3),
            accesses_per_s: 1.0e6,
            instructions_per_s: Some(1.6e10),
            ..Measured::default()
        };
        let next = f.step(&Knobs::default(), &m, &goal);
        assert_eq!(next.seq, 0.0, "{next:?}");
    }

    #[test]
    fn compute_floor_scales_the_access_up_when_ipc_is_still_high() {
        // fio-like at the floors: no compute left to slow, a visit on a
        // share of accesses, IPC still twice the target. Both compute and
        // the visit share grow, so misses per instruction hold.
        let mut f = Fitter::new(1, LIMITS);
        let goal = Goal {
            ipc: Some(0.89),
            mpki: Some(0.018),
            dtlb_pki: Some(8.7),
            ..Goal::default()
        };
        let k = Knobs {
            ops: 1,
            chains: 0.25,
            lines: 0.5,
            tlbvisits: 2000.0,
            sparse_pages: 2048,
            ..Knobs::default()
        };
        let m = Measured {
            ipc: Some(2.5),
            mpki: Some(0.018),
            dtlb_pki: Some(10.0),
            accesses_per_s: 2.0e4,
            instructions_per_s: Some(1.0e9),
            ..Measured::default()
        };
        f.last_ipc = Some(2.5);
        let next = f.step(&k, &m, &goal);
        let up = (1.0 / factor(0.89, 2.5, 0.5)).powi(2).min(2.0);
        assert!(next.ops > 1, "{next:?}");
        assert!(
            (next.lines / k.lines - up).abs() < 1e-9,
            "visits scale by the same factor as compute: {next:?}"
        );
    }

    #[test]
    fn bandwidth_takes_the_write_share_first_and_non_temporal_lines_only_at_high_ipc() {
        let goal = Goal {
            ipc: Some(0.3),
            mpki: Some(20.0),
            dram: Some(4.0e10),
            ..Goal::default()
        };
        let k = Knobs {
            ops: 1,
            lines: 4.0,
            wfrac: 0.25,
            ..Knobs::default()
        };
        let m = Measured {
            ipc: Some(0.3),
            mpki: Some(20.0),
            dram: Some(1.0e10),
            accesses_per_s: 1.0e7,
            instructions_per_s: Some(2.0e9),
            ..Measured::default()
        };
        let mut f = Fitter::new(4, LIMITS);
        let k1 = f.step(&k, &m, &goal);
        assert!(k1.wfrac > 0.25 && k1.ntlines == 0.0, "{k1:?}");
        let full = Knobs {
            wfrac: 1.0,
            ..k.clone()
        };
        let k2 = f.step(&full, &m, &goal);
        assert_eq!(
            k2.ntlines, 0.0,
            "a low-IPC profile waits rather than pay IPC: {k2:?}"
        );
        let high = Goal {
            ipc: Some(1.2),
            ..goal.clone()
        };
        let m_high = Measured {
            ipc: Some(1.2),
            ..m.clone()
        };
        let k3 = f.step(&full, &m_high, &high);
        assert!(k3.ntlines > 0.0, "{k3:?}");
    }

    #[test]
    fn dilution_regime_reaches_a_high_tlb_rate() {
        // fio-like: almost no misses, a high dTLB rate. The run length
        // falls below one visit per access instead of compute growing
        // without end.
        let truth = Knobs {
            ops: 300,
            chains: 1.0,
            lines: 0.05,
            tlbvisits: 9.0,
            sparse_pages: 2048,
            code_pages: 16384,
            ..Knobs::default()
        };
        let goal = goal_of(&plant(&truth, 1));
        let (f, k, statuses) = converge(&goal, 1, TlbRegime::L2Hit, 40);
        assert!(Fitter::converged(&statuses), "{statuses:#?}\nknobs {k:#?}");
        // The ratios allow heavier accesses than the truth's; what must
        // not happen is compute growing without end.
        assert!(
            f64::from(k.ops) * k.chains.max(1.0) < 1.0e5,
            "{k:?} after {} passes",
            f.passes
        );
    }

    #[test]
    fn partial_goals_only_fit_what_is_asked() {
        let goal = Goal {
            cores: Some(2.0),
            ipc: Some(1.0),
            ..Goal::default()
        };
        assert!(!goal.is_empty());
        assert!(Goal::default().is_empty());
        let (_, k, statuses) = converge(&goal, 4, TlbRegime::L2Hit, 40);
        assert!(Fitter::converged(&statuses), "{statuses:#?}");
        assert_eq!(statuses.len(), 2);
        assert_eq!(k.ntlines, 0.0);
        assert_eq!(k.tlbvisits, 0.0);
        assert_eq!(k.codevisits, 0.0);
    }

    #[test]
    fn unmeasured_targets_do_not_block_convergence() {
        let goal = Goal {
            cores: Some(1.0),
            dram: Some(1.0e9),
            ..Goal::default()
        };
        let f = Fitter::new(2, LIMITS);
        let m = Measured {
            cores: Some(1.0),
            dram: None,
            ..Measured::default()
        };
        let s = f.statuses(&m, &goal);
        assert_eq!(s.len(), 2);
        assert!(s[0].within);
        assert_eq!(s[1].measured, None);
        assert!(!s[1].within);
        assert!(
            Fitter::converged(&s),
            "unmeasured dram is not a failure to converge"
        );
    }

    #[test]
    fn infeasible_goal_runs_out_of_passes_without_nonsense() {
        let goal = Goal {
            ipc: Some(3.0),
            mpki: Some(80.0),
            ..Goal::default()
        };
        let (f, k, statuses) = converge(&goal, 2, TlbRegime::L2Hit, 15);
        assert_eq!(f.passes, 15);
        assert!(!Fitter::converged(&statuses));
        assert!(k.ops >= 1 && (1.0..=64.0).contains(&k.lines));
        assert!(statuses.iter().all(|s| s.error.is_some_and(f64::is_finite)));
        let worst = statuses
            .iter()
            .map(|s| s.error.unwrap().abs())
            .fold(0.0, f64::max);
        assert!(worst > 0.05);
    }

    #[test]
    fn statuses_and_errors() {
        let f = Fitter::new(1, LIMITS);
        let goal = Goal {
            ipc: Some(2.0),
            itlb_pki: Some(0.0),
            ..Goal::default()
        };
        let s = f.statuses(
            &Measured {
                ipc: Some(2.08),
                itlb_pki: Some(0.0),
                ..Measured::default()
            },
            &goal,
        );
        assert!((s[0].error.unwrap() - 0.04).abs() < 1e-12);
        assert!(s[0].within);
        assert_eq!(s[1].error, Some(0.0));
        let s = f.statuses(
            &Measured {
                ipc: Some(2.2),
                itlb_pki: Some(0.1),
                ..Measured::default()
            },
            &goal,
        );
        assert!(!s[0].within);
        assert_eq!(s[1].error, Some(f64::INFINITY));
        assert!(!s[1].within);
        let near_zero = Goal {
            itlb_pki: Some(0.000_003),
            dtlb_pki: Some(0.0001),
            mpki: Some(0.006),
            ..Goal::default()
        };
        let s = f.statuses(
            &Measured {
                itlb_pki: Some(0.000_02),
                dtlb_pki: Some(0.015),
                mpki: Some(0.04),
                ..Measured::default()
            },
            &near_zero,
        );
        assert!(s.iter().all(|r| r.within), "{s:#?}");
        assert!(
            s.iter().all(|r| r.error.unwrap() > 1.0),
            "ratios are huge but meaningless"
        );
        assert_eq!(factor(2.0, 1.0, 0.5), 2.0f64.sqrt());
        assert_eq!(factor(1.0, 0.0, 0.5), 1.0, "no measurement, no move");
        assert_eq!(factor(100.0, 1.0, 1.0), 4.0, "clamped");
    }

    #[test]
    fn duty_and_burstiness_come_from_the_clipped_model() {
        assert_eq!(Fitter::duty_cv_knobs(0.3, 0.0), (0.3, 0.0));
        assert_eq!(Fitter::duty_cv_knobs(0.0, 0.5), (0.0, 0.0));
        let (duty, knob) = Fitter::duty_cv_knobs(0.3, 0.6);
        let (m, c) = clipped_duty_stats(duty, knob);
        assert!(
            (m - 0.3).abs() < 0.01 && (c - 0.6).abs() < 0.03,
            "{duty} {knob} -> {m} {c}"
        );
        assert!(
            duty > 0.3 && knob > 0.6,
            "both knobs rise to make up for the clip"
        );
        // A CV the mean cannot carry: the mean is kept and the CV falls
        // as short as it must.
        for (mean, cv) in [(0.6, 0.6), (0.95, 1.5)] {
            let (duty, knob) = Fitter::duty_cv_knobs(mean, cv);
            assert!(duty <= 1.0 && knob <= 3.0);
            let (m, c) = clipped_duty_stats(duty, knob);
            assert!((m - mean).abs() < 0.01, "the mean is met: {m} for {mean}");
            assert!(c < cv && c > 0.0, "the CV falls short: {c} for {cv}");
        }
    }

    #[test]
    fn the_first_guess_reads_the_shape_and_distance_ranks_passes() {
        let f = Fitter::new(4, LIMITS);
        let hot = f.initial_knobs(
            &Goal {
                ipc: Some(0.22),
                mpki: Some(20.0),
                ..Goal::default()
            },
            TlbRegime::L2Hit,
        );
        assert!(
            hot.ops == 64 && hot.dep == 0.0 && hot.chains == 0.5,
            "{hot:?}"
        );
        let cold = f.initial_knobs(
            &Goal {
                ipc: Some(3.7),
                mpki: Some(0.002),
                ..Goal::default()
            },
            TlbRegime::L2Hit,
        );
        assert!(cold.ops == 4096 && cold.dep == 0.0, "{cold:?}");
        assert_eq!(f.initial_knobs(&Goal::default(), TlbRegime::L2Hit).ops, 64);
        let goal = Goal {
            ipc: Some(1.0),
            mpki: Some(10.0),
            cores: Some(2.0),
            ..Goal::default()
        };
        let near = f.statuses(
            &Measured {
                ipc: Some(1.03),
                mpki: Some(10.2),
                cores: Some(2.0),
                ..Measured::default()
            },
            &goal,
        );
        let far = f.statuses(
            &Measured {
                ipc: Some(1.3),
                mpki: Some(10.0),
                cores: Some(2.0),
                ..Measured::default()
            },
            &goal,
        );
        assert!(f.distance(&near) < 1.0 && f.distance(&near) > 0.0);
        assert!(
            (f.distance(&far) - 6.0).abs() < 1e-9,
            "30% over a 5% tolerance"
        );
        let floor = f.statuses(
            &Measured {
                ipc: Some(1.0),
                mpki: Some(10.0),
                cores: Some(2.03),
                ..Measured::default()
            },
            &goal,
        );
        assert_eq!(
            f.distance(&floor),
            0.0,
            "within the absolute floor counts as met"
        );
    }

    #[test]
    fn threads_and_initial_guess() {
        let mut t = Targets::new();
        assert_eq!(Fitter::threads_for(&t, 64), 1);
        t.set("cpu_usage_cores", 2.3).unwrap();
        assert_eq!(Fitter::threads_for(&t, 64), 3);
        t.set("cpu_usage_cv_1s", 0.5).unwrap();
        assert_eq!(
            Fitter::threads_for(&t, 64),
            4,
            "headroom for bursts: 2.3 * 1.5"
        );
        t.set("cpu_limit_cores", 8.0).unwrap();
        assert_eq!(Fitter::threads_for(&t, 64), 8);
        assert_eq!(Fitter::threads_for(&t, 4), 4, "capped by available CPUs");
        let f = Fitter::new(
            8,
            Limits {
                sparse_capacity: 100,
                sparse_floor: 128,
                code_capacity: 50,
                main_pages: 256,
                kreads_available: false,
            },
        );
        let k = f.initial_knobs(
            &Goal {
                cores: Some(2.0),
                ipc: Some(2.5),
                dtlb_pki: Some(1.0),
                ..Goal::default()
            },
            TlbRegime::Walk,
        );
        assert_eq!(k.duty, 0.25);
        assert_eq!(k.chains, 4.0);
        assert_eq!(k.sparse_pages, 100, "capped by capacity");
        assert_eq!(k.code_pages, 50);
        assert!(k.tlbvisits > 0.0 && k.codevisits == 0.0);
        let b = Buffers::new(1 << 20, false, 1000, 50).unwrap();
        k.validate(&b).unwrap();
        let g = f.initial_knobs(
            &Goal {
                ipc: Some(1.0),
                ..Goal::default()
            },
            TlbRegime::L2Hit,
        );
        assert_eq!((g.chains, g.sparse_pages, g.duty), (1.0, 100, 1.0));
        assert_eq!(Goal::from_targets(&t).cores, Some(2.3));
    }

    proptest! {
        #[test]
        fn steps_stay_valid(
            duty in 0.0..=1.0f64, cv in 0.0..2.0f64, ops in 1u32..10000, lines in 1.0..=64.0f64,
            nt in 0.0..100.0f64, tv in 0.0..10.0f64, cvis in 0.0..10.0f64,
            cores in 0.1..64.0f64, ipc in 0.1..5.0f64, mpki in 0.0..100.0f64, dram in 0.0..1e11f64,
            dtlb in 0.0..50.0f64, itlb in 0.0..10.0f64, aps in 0.0..1e8f64,
        ) {
            let mut f = Fitter::new(4, LIMITS);
            let k = Knobs { duty, cv, ops, chains: 2.0, lines, ntlines: nt, tlbvisits: tv, codevisits: cvis, sparse_pages: 2048, code_pages: 16384, ..Knobs::default() };
            let goal = Goal { cores: Some(2.0), cv: Some(0.3), ipc: Some(1.2), mpki: Some(5.0), dram: Some(4e9), dtlb_pki: Some(2.0), itlb_pki: Some(0.1) };
            let m = Measured { cores: Some(cores), cv: Some(cv), ipc: Some(ipc), mpki: Some(mpki), dram: Some(dram), dtlb_pki: Some(dtlb), itlb_pki: Some(itlb), accesses_per_s: aps, cv_samples: 300, instructions_per_s: Some(aps * 100.0) };
            let next = f.step(&k, &m, &goal);
            let b = Buffers::new(1 << 20, false, 2048, 16384).unwrap();
            prop_assert!(next.validate(&b).is_ok(), "{next:?}");
            prop_assert!(next.ops >= 1 && next.ntlines <= NTLINES_MAX);
        }
    }
}
