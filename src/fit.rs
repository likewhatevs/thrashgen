//! Fitting knobs to targets: `docs/design.md` section 6. The rules are
//! pure functions of the last window's measurements so they can be
//! driven against a simulated machine in tests.

use crate::loadgen::Knobs;
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
}

const OPS_RANGE: (f64, f64) = (1.0, 1.0e6);
/// One-second samples a window needs before its burstiness is trusted.
pub const MIN_CV_SAMPLES: usize = 30;
const NTLINES_MAX: u32 = 512;
const VISITS_RANGE: (f64, f64) = (1.0e-4, 64.0);

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
            Some(i) if i < 1.4 => 1,
            Some(i) if i < 2.2 => 2,
            Some(_) => 4,
            None => 2,
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
            wfrac: 0.25,
            ntlines: 0,
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

    /// Compare a window against the goal.
    pub fn statuses(&self, m: &Measured, goal: &Goal) -> Vec<Status> {
        let rows: [(&'static str, Option<f64>, Option<f64>, f64); 7] = [
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
                Some(Status {
                    key,
                    target,
                    measured,
                    error,
                    within: error.is_some_and(|e| e.abs() <= tol),
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

    /// One pass: move each knob by its correction ratio, damped.
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

        let work_before = f64::from(knobs.ops) * f64::from(knobs.chains);
        if let (Some(t), Some(v)) = (goal.mpki, m.mpki) {
            // More instructions per access dilute the misses.
            k.ops = (f64::from(k.ops) * factor(v, t, d))
                .clamp(OPS_RANGE.0, OPS_RANGE.1)
                .round() as u32;
        }
        if let (Some(t), Some(v)) = (goal.ipc, m.ipc) {
            // Longer runs let the prefetcher stream, raising IPC at equal
            // MPKI; ops is left to the MPKI rule so the two do not fight,
            // unless MPKI is not a target and compute is free to grow.
            let g = factor(t, v, d);
            if goal.mpki.is_none() {
                k.ops = (f64::from(k.ops) * g)
                    .clamp(OPS_RANGE.0, OPS_RANGE.1)
                    .round() as u32;
            }
            k.lines = (k.lines * g).clamp(1.0, 64.0);
            // Chain count is the coarse step, taken only once the fine
            // knob has nothing left to give.
            if k.lines >= 64.0 && g > 1.0 + self.tol.ratio {
                k.chains = match k.chains {
                    1 => 2,
                    _ => 4,
                };
            } else if k.lines <= 1.0 && g < 1.0 - self.tol.ratio {
                k.chains = match k.chains {
                    4 => 2,
                    _ => 1,
                };
            }
        }

        // Instructions per access scale with the compute work; the
        // per-access rates are pre-scaled so their per-instruction
        // rates hold, and only then corrected on their own.
        let work_after = f64::from(k.ops) * f64::from(k.chains);
        let q = if work_before > 0.0 {
            work_after / work_before
        } else {
            1.0
        };
        k.tlbvisits *= q;
        k.codevisits *= q;
        k.ntlines = (f64::from(k.ntlines) * q)
            .round()
            .clamp(0.0, f64::from(NTLINES_MAX)) as u32;

        if let (Some(t), Some(v)) = (goal.dram, m.dram) {
            if m.accesses_per_s > 0.0 {
                let per_line = 64.0 * m.accesses_per_s;
                let nt_now = f64::from(k.ntlines) * per_line;
                let other = (v - nt_now).max(0.0);
                let need = ((t - other) / per_line).max(0.0);
                let next = f64::from(k.ntlines) + d * (need - f64::from(k.ntlines));
                k.ntlines = next.round().clamp(0.0, f64::from(NTLINES_MAX)) as u32;
            }
        }
        if let (Some(t), Some(v)) = (goal.dtlb_pki, m.dtlb_pki) {
            k.tlbvisits = if k.tlbvisits > 0.0 {
                (k.tlbvisits * factor(t, v, d)).clamp(VISITS_RANGE.0, VISITS_RANGE.1)
            } else if t > 0.0 {
                VISITS_RANGE.0 * 100.0
            } else {
                0.0
            };
            if t == 0.0 {
                k.tlbvisits = 0.0;
            }
        }
        if let (Some(t), Some(v)) = (goal.itlb_pki, m.itlb_pki) {
            k.codevisits = if k.codevisits > 0.0 {
                (k.codevisits * factor(t, v, d)).clamp(VISITS_RANGE.0, VISITS_RANGE.1)
            } else if t > 0.0 {
                VISITS_RANGE.0 * 100.0
            } else {
                0.0
            };
            if t == 0.0 {
                k.codevisits = 0.0;
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
    };

    /// A stand-in machine: monotone in every knob the way the real one
    /// is expected to be, with made-up constants.
    fn plant(k: &Knobs, threads: usize) -> Measured {
        let chains_eff = match k.chains {
            1 => 1.0,
            2 => 1.6,
            _ => 2.8,
        };
        let ops = f64::from(k.ops);
        let lines = k.lines;
        let nt = f64::from(k.ntlines);
        let instr = 2.0 * ops * f64::from(k.chains)
            + 3.0 * lines
            + 4.0 * nt
            + 2.0 * k.tlbvisits
            + 3.0 * k.codevisits
            + 12.0;
        let misses = 0.95 * lines + 0.6;
        let stall = misses * 250.0 / (1.0 + 0.3 * lines);
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
            dtlb_pki: Some(1000.0 * k.tlbvisits * hit / instr),
            itlb_pki: Some(1000.0 * k.codevisits * walk / instr),
            accesses_per_s: aps,
            cv_samples: 300,
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
                    chains: 1,
                    lines: 4.0,
                    ntlines: 3,
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
                    chains: 4,
                    lines: 12.0,
                    ntlines: 0,
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
                    chains: 2,
                    lines: 16.0,
                    ntlines: 8,
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
        assert_eq!(k.ntlines, 0);
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
        assert_eq!(k.chains, 4);
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
        assert_eq!((g.chains, g.sparse_pages, g.duty), (1, 1000, 1.0));
        assert_eq!(Goal::from_targets(&t).cores, Some(2.3));
    }

    proptest! {
        #[test]
        fn steps_stay_valid(
            duty in 0.0..=1.0f64, cv in 0.0..2.0f64, ops in 1u32..10000, lines in 1.0..=64.0f64,
            nt in 0u32..100, tv in 0.0..10.0f64, cvis in 0.0..10.0f64,
            cores in 0.1..64.0f64, ipc in 0.1..5.0f64, mpki in 0.0..100.0f64, dram in 0.0..1e11f64,
            dtlb in 0.0..50.0f64, itlb in 0.0..10.0f64, aps in 0.0..1e8f64,
        ) {
            let mut f = Fitter::new(4, LIMITS);
            let k = Knobs { duty, cv, ops, chains: 2, lines, ntlines: nt, tlbvisits: tv, codevisits: cvis, sparse_pages: 2048, code_pages: 16384, ..Knobs::default() };
            let goal = Goal { cores: Some(2.0), cv: Some(0.3), ipc: Some(1.2), mpki: Some(5.0), dram: Some(4e9), dtlb_pki: Some(2.0), itlb_pki: Some(0.1) };
            let m = Measured { cores: Some(cores), cv: Some(cv), ipc: Some(ipc), mpki: Some(mpki), dram: Some(dram), dtlb_pki: Some(dtlb), itlb_pki: Some(itlb), accesses_per_s: aps, cv_samples: 300 };
            let next = f.step(&k, &m, &goal);
            let b = Buffers::new(1 << 20, false, 2048, 16384).unwrap();
            prop_assert!(next.validate(&b).is_ok(), "{next:?}");
            prop_assert!(next.ops >= 1 && next.ntlines <= NTLINES_MAX);
        }
    }
}
