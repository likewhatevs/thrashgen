//! Fitting knobs to targets: `docs/design.md` section 6. The rules are
//! pure functions of the last window's measurements so they can be
//! driven against a simulated machine in tests.

use crate::loadgen::{Knobs, SLOWEST_CHAINS};
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
    /// Code pages available.
    pub code_capacity: usize,
    /// Pages in the main buffer, for the working-set floor.
    pub main_pages: usize,
}

const OPS_RANGE: (f64, f64) = (1.0, 1.0e6);
/// One-second samples a window needs before its burstiness is trusted.
pub const MIN_CV_SAMPLES: usize = 30;

/// Absolute tolerance per field: a target near zero is met when the
/// measurement is near zero too, whatever the ratio says. Memory gets
/// two hugepages of slack for process-level accounting noise.
pub fn absolute_tolerance(key: &str) -> f64 {
    match key {
        "cpu_usage_cores" => 0.05,
        "cpu_usage_cv_1s" => 0.02,
        "ipc" => 0.02,
        "l3_mpki" => 0.05,
        "dram_bytes_per_sec" => 1.0e8,
        "dtlb_pki" => 0.02,
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
/// MPKI from which the stall on misses, not compute, sets IPC.
const MEMORY_BOUND_MPKI: f64 = 1.0;
/// Smallest run length: one visit per 1024 accesses.
const LINES_MIN: f64 = 1.0 / 1024.0;
/// How much of the random visits turn dependent per pass when IPC is
/// high in a memory-bound regime.
const DEP_STEP: f64 = 0.2;

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
    /// Passes taken so far.
    pub passes: u32,
    /// Last pass's compute work (ops times chains) and instructions per
    /// access, for the secant slope.
    pub last_work_ipa: Option<(f64, f64)>,
    /// Last pass's IPC, to notice when line length stops moving it.
    pub last_ipc: Option<f64>,
    /// Shrinking the working set cost misses once, so the dTLB rule
    /// uses the sequential share from then on.
    pub wset_blocked: bool,
    /// Last pass's working set, run length and misses per access, to
    /// tell a shrink that cost misses from one that did not.
    pub last_set: Option<(f64, f64, f64)>,
}

/// A correction ratio raised to the damping, kept to a sane range.
fn factor(target: f64, measured: f64, damping: f64) -> f64 {
    if measured.is_nan() || target.is_nan() || measured <= 0.0 || target <= 0.0 {
        return 1.0;
    }
    (target / measured).powf(damping).clamp(0.25, 4.0)
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
            last_set: None,
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

    /// A first guess from the goal alone.
    pub fn initial_knobs(&self, goal: &Goal, regime: TlbRegime) -> Knobs {
        let cores = goal.cores.unwrap_or(self.threads as f64);
        let chains = match goal.ipc {
            Some(i) if i < 0.5 => 0.5,
            Some(i) if i < 1.4 => 1.0,
            Some(i) if i < 2.2 => 2.0,
            Some(_) => 4.0,
            None => 2.0,
        };
        let sparse_pages = match regime {
            TlbRegime::L2Hit => 2048,
            TlbRegime::Walk => 16384,
        }
        .min(self.limits.sparse_capacity)
        .max(1);
        Knobs {
            duty: (cores / self.threads as f64).clamp(0.0, 1.0),
            cv: goal.cv.unwrap_or(0.0),
            ops: 64,
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
        self.passes += 1;
        let d = self.damping;
        let mut k = knobs.clone();

        if let (Some(t), Some(v)) = (goal.cores, m.cores) {
            k.duty = (k.duty * factor(t, v, d)).clamp(0.0, 1.0);
        }
        if let Some(t) = goal.cv {
            // A window needs enough one-second samples before its CV
            // means anything, and the knob stays within twice the target
            // so clipping at full duty cannot run it away.
            k.cv = match m.cv {
                Some(v) if v > 0.0 && k.cv > 0.0 && m.cv_samples >= MIN_CV_SAMPLES => {
                    (k.cv * factor(t, v, d)).clamp(0.0, 2.0 * t)
                }
                _ => t,
            };
        }

        // Instructions per access now, and the slope against compute
        // work, which is ops times chains.
        let ipa = m.instructions_per_access();
        // Below one chain the slow iterations cost about what one-chain
        // iterations do in instructions, so work does not shrink with
        // the knob there.
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
        if let Some(ipa) = ipa {
            self.last_work_ipa = Some((work_now, ipa));
        }

        let mut work = work_now;
        if let (Some(t), Some(v)) = (goal.ipc, m.ipc) {
            // Longer runs let the prefetcher stream, raising IPC at equal
            // MPKI; more chains raise the compute IPC. Run length is the
            // fine knob; chains move once it is at its range or the last
            // pass failed to move IPC by the tolerance, which is what a
            // compute-bound regime looks like. With no MPKI target,
            // compute work is free to carry IPC too.
            let g = factor(t, v, d);
            if goal.mpki.is_none() {
                work *= g;
            }
            let stalled = self
                .last_ipc
                .is_some_and(|prev| (v - prev).abs() < prev * self.tol.ratio);
            let exhausted = (g > 1.0 && k.lines >= 64.0) || (g < 1.0 && k.lines <= 1.0);
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
            let memory_bound = m.mpki.is_some_and(|x| x >= MEMORY_BOUND_MPKI);
            // The shares are given back first: a sequential walk when IPC
            // is high, dependent misses when it is short. A small share
            // goes back whole and the ladder continues this pass.
            let mut handled = false;
            if g < 1.0 && k.seq > 0.0 {
                handled = k.seq >= SHARE_SMALL;
                k.seq = if handled { k.seq * g } else { 0.0 };
            } else if g > 1.0 && k.dep > 0.0 {
                handled = k.dep >= SHARE_SMALL;
                k.dep = if handled { (k.dep / g).min(1.0) } else { 0.0 };
            }
            let lines_can_move = if g > 1.0 {
                k.lines < 64.0
            } else {
                k.lines > 1.0 && !lines_locked
            };
            let dep_up = |k: &mut Knobs| k.dep = (k.dep + DEP_STEP).min(1.0);
            let chains_move = |k: &mut Knobs| k.chains = (k.chains * g).clamp(SLOWEST_CHAINS, 4.0);
            let lines_move = |k: &mut Knobs| k.lines = (k.lines * g).clamp(1.0, 64.0);
            if !handled && g < 1.0 && memory_bound {
                // Stalls set IPC here: serialize the misses once compute is
                // down to a chain or has stopped helping, else cut it.
                if k.dep < 1.0 && (k.chains <= 1.0 || stalled || knobs.ops <= 1) {
                    dep_up(&mut k);
                } else if chains_can_move {
                    chains_move(&mut k);
                } else if lines_can_move {
                    lines_move(&mut k);
                }
            } else if !handled && g < 1.0 {
                // Compute sets IPC: shorter runs, then fewer chains, and
                // dependent misses as the last resort.
                if lines_can_move && !stalled {
                    lines_move(&mut k);
                } else if chains_can_move {
                    chains_move(&mut k);
                } else {
                    dep_up(&mut k);
                }
            } else if !handled {
                // IPC short: longer runs, more chains, then a sequential
                // walk that lets the prefetcher stream.
                if lines_can_move && !(stalled && !exhausted) {
                    lines_move(&mut k);
                } else if chains_can_move {
                    chains_move(&mut k);
                } else if k.seq < 1.0 {
                    k.seq = (k.seq.max(SHARE_SEED) * g).clamp(0.0, 1.0);
                }
            }
            self.last_ipc = Some(v);
        }
        if knobs.ops > OPS_DILUTION_CAP && k.lines > LINES_MIN && goal.mpki.is_some() {
            k.lines = (k.lines * 0.5).max(LINES_MIN);
        }
        match (goal.mpki, m.mpki, ipa) {
            (Some(t), Some(v), Some(ipa)) if t > 0.0 && v > 0.0 => {
                // Misses per access follow the run length the IPC rule just
                // chose; at that length, the instructions per access the
                // target needs are known, and the work that gives them.
                let misses_per_access = v / 1000.0 * ipa * (k.lines / knobs.lines.max(1.0));
                let want_ipa = misses_per_access * 1000.0 / t;
                work += d * (want_ipa - ipa) / slope;
            }
            (Some(t), Some(v), None) => work *= factor(v, t, d),
            _ => {}
        }
        k.ops = (work / k.chains.max(1.0))
            .clamp(OPS_RANGE.0, OPS_RANGE.1)
            .round() as u32;
        if let (Some(t), Some(v)) = (goal.mpki, m.mpki) {
            // Compute cannot go below one op per access; past that, more
            // lines per access are the only way to more misses.
            if k.ops <= 1 && knobs.ops <= 1 && v < t * (1.0 - self.tol.ratio) {
                k.lines = (k.lines * factor(t, v, d)).clamp(LINES_MIN, 64.0);
            }
        }

        // Instructions per access after this pass, by the same model, and
        // the access rate it implies at unchanged cycles per instruction.
        let ipa_next =
            ipa.map(|i| (i + slope * (f64::from(k.ops) * k.chains.max(1.0) - work_now)).max(1.0));
        let aps_next = match (ipa, ipa_next) {
            (Some(i), Some(n)) if m.accesses_per_s > 0.0 => m.accesses_per_s * i / n,
            _ => m.accesses_per_s,
        };

        if let (Some(t), Some(v)) = (goal.dram, m.dram) {
            if aps_next > 0.0 {
                // The lines the target needs at next pass's access rate, by
                // the model; damped linearly since it is an absolute estimate.
                let per_line_now = 64.0 * m.accesses_per_s;
                let nt_now = k.ntlines * per_line_now;
                let other_per_access = (v - nt_now).max(0.0) / m.accesses_per_s.max(1e-9);
                let need_lines = ((t / aps_next - other_per_access) / 64.0).max(0.0);
                let now = k.ntlines;
                k.ntlines = (now + d * (need_lines - now)).clamp(0.0, NTLINES_MAX);
            }
        }
        // Visits per access may grow with the access itself: a visit is a
        // few instructions, so a quarter of them is the ceiling.
        let visits_max = VISITS_RANGE.1.max(ipa_next.unwrap_or(0.0) / 4.0);
        let rate_rule = |target: Option<f64>, measured: Option<f64>, visits: f64| -> f64 {
            let (Some(t), Some(v)) = (target, measured) else {
                return visits;
            };
            if t <= 0.0 {
                return 0.0;
            }
            match (ipa, ipa_next) {
                (Some(ipa), Some(next)) if visits > 0.0 && v > 0.0 => {
                    // Full compensation for the change in instructions per
                    // access, which the model predicts, then a damped
                    // correction of the measured error.
                    (visits * (next / ipa) * factor(t, v, d)).clamp(VISITS_RANGE.0, visits_max)
                }
                _ if visits > 0.0 => (visits * factor(t, v, d)).clamp(VISITS_RANGE.0, visits_max),
                _ => VISITS_RANGE.0 * 100.0,
            }
        };
        k.tlbvisits = rate_rule(goal.dtlb_pki, m.dtlb_pki, k.tlbvisits);
        k.codevisits = rate_rule(goal.itlb_pki, m.itlb_pki, k.codevisits);

        // A shrink of the working set that cost misses, beyond what the
        // run length explains, is undone and not tried again: misses
        // have priority, and dTLB falls to the sequential share.
        let mpa_now = m.mpki.zip(ipa).map(|(v, ipa)| v / 1000.0 * ipa);
        if let (Some((wset_prev, lines_prev, mpa_prev)), Some(mpa)) = (self.last_set, mpa_now) {
            if knobs.wset < wset_prev && mpa < 0.6 * mpa_prev * (knobs.lines / lines_prev.max(1.0))
            {
                k.wset = wset_prev;
                self.wset_blocked = true;
            }
        }
        if let Some(mpa) = mpa_now {
            self.last_set = Some((knobs.wset, knobs.lines, mpa));
        }
        // The main stream's own pages cause dTLB events the sparse stream
        // cannot take back. When they alone exceed the target, touch
        // fewer pages, or walk them in order when the set cannot shrink
        // or IPC is short and would gain from streaming.
        if let (Some(t), Some(v), Some(ipa)) = (goal.dtlb_pki, m.dtlb_pki, ipa) {
            let sparse_at_most = 1000.0 * knobs.tlbvisits / ipa;
            let excess = v - sparse_at_most;
            let abs_tol = absolute_tolerance("dtlb_pki");
            if excess > t * (1.0 + self.tol.ratio) + abs_tol {
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
                } else if !ipc_high {
                    // A walk would raise IPC further; IPC has priority.
                    k.seq = (k.seq.max(SHARE_SEED) / f).clamp(0.0, 1.0);
                }
            }
        }
        k.sparse_pages = k.sparse_pages.clamp(1, self.limits.sparse_capacity.max(1));
        k.code_pages = k.code_pages.clamp(1, self.limits.code_capacity.max(1));
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loadgen::Buffers;
    use proptest::prelude::*;

    const LIMITS: Limits = Limits {
        sparse_capacity: 16384,
        code_capacity: 16384,
        main_pages: 262_144,
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
        // Overlapped misses cost a fraction of their latency; dependent
        // ones the whole of it; a sequential walk is mostly prefetched.
        let overlap = (0.3 + 0.7 * k.dep) * (1.0 - 0.6 * k.seq);
        let stall = misses * 250.0 * overlap / (1.0 + 0.3 * lines);
        let main_tlb = ((1.0 - k.seq) + k.seq * lines / 64.0) * tlb_rate * lines.min(1.0);
        let cycles = ops * 4.0 / chains_eff + stall + 10.0;
        let duty = k.duty;
        let aps = threads as f64 * duty * 3.0e9 / cycles;
        let hit = if k.sparse_pages < 96 {
            0.05
        } else if k.sparse_pages <= 3072 {
            0.95
        } else {
            0.1
        };
        let walk = if k.code_pages > 1024 { 0.9 } else { 0.05 };
        Measured {
            cores: Some(threads as f64 * duty * 1.02),
            cv: Some(k.cv * if duty > 0.9 { 0.7 } else { 1.0 }),
            ipc: Some(instr / cycles),
            mpki: Some(1000.0 * misses / instr),
            dram: Some((misses * 64.0 + nt * 64.0 + k.wfrac * lines * 32.0) * aps),
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
                    ntlines: 3.0,
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
            assert!(
                Fitter::converged(&statuses),
                "{name}: {statuses:#?}\nknobs {k:#?}"
            );
            assert!(shape(&k), "{name}: knobs {k:#?} after {} passes", f.passes);
        }
    }

    #[test]
    fn a_shrink_that_costs_misses_is_undone_and_blocked() {
        let mut f = Fitter::new(4, LIMITS);
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
        assert!(k2.seq > 0.0, "dTLB excess goes to the sequential share now");
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
        assert!(k.lines < 1.0, "{k:?} after {} passes", f.passes);
        assert!(k.ops < OPS_DILUTION_CAP, "{k:?}");
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
                sparse_capacity: 1000,
                code_capacity: 50,
                main_pages: 256,
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
        assert_eq!(k.sparse_pages, 1000, "capped by capacity");
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
        assert_eq!((g.chains, g.sparse_pages, g.duty), (1.0, 1000, 1.0));
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
