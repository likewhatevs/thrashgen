//! The load generator: worker threads that repeat one *access* inside
//! a duty-cycled period. `docs/design.md` section 5 maps each knob to
//! the observable it moves.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::sched::{CpuSet, sched_setaffinity};
use nix::sys::mman::{ProtFlags, mprotect};
use nix::unistd::Pid;
use rand::{RngExt, SeedableRng};
use rand_distr::{Distribution, LogNormal};
use rand_xoshiro::Xoshiro256PlusPlus;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::footprint::AnonBuffer;

/// Cache line size the streams assume.
pub const LINE: usize = 64;
/// Small page size the streams assume.
pub const PAGE: usize = 4096;
/// Accesses between clock checks.
const BATCH: u32 = 64;

/// What the workers do, all adjustable while they run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Knobs {
    /// Busy fraction of each period, 0 to 1.
    pub duty: f64,
    /// Coefficient of variation of the per-second duty factor.
    pub cv: f64,
    /// Compute iterations per access.
    pub ops: u32,
    /// Independent dependency chains in the compute loop: 1, 2 or 4.
    pub chains: u8,
    /// Consecutive lines touched per main-stream visit, 1 to 64.
    pub lines: u32,
    /// Fraction of main-stream lines that are stored to, 0 to 1.
    pub wfrac: f64,
    /// Lines written with non-temporal stores per access.
    pub ntlines: u32,
    /// Sparse-stream loads per access.
    pub tlbvisits: u32,
    /// Pages the sparse stream picks from; at most the buffer's capacity.
    pub sparse_pages: usize,
    /// Stub calls per access; may be below one.
    pub codevisits: f64,
    /// Executable pages the code stream picks from; at most the capacity.
    pub code_pages: usize,
    /// Period length.
    pub period_ms: u64,
}

impl Default for Knobs {
    fn default() -> Self {
        Knobs {
            duty: 1.0,
            cv: 0.0,
            ops: 64,
            chains: 2,
            lines: 4,
            wfrac: 0.25,
            ntlines: 0,
            tlbvisits: 0,
            sparse_pages: 1,
            codevisits: 0.0,
            code_pages: 1,
            period_ms: 10,
        }
    }
}

/// Invalid knob values.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct KnobError(String);

impl Knobs {
    /// Check ranges against the buffers the workers will use.
    pub fn validate(&self, buffers: &Buffers) -> Result<(), KnobError> {
        let unit = |v: f64, name: &str| {
            if (0.0..=1.0).contains(&v) {
                Ok(())
            } else {
                Err(KnobError(format!("{name} must be in 0..=1, got {v}")))
            }
        };
        unit(self.duty, "duty")?;
        unit(self.wfrac, "wfrac")?;
        if !(self.cv >= 0.0 && self.cv.is_finite()) {
            return Err(KnobError(format!(
                "cv must be finite and non-negative, got {}",
                self.cv
            )));
        }
        if !matches!(self.chains, 1 | 2 | 4) {
            return Err(KnobError(format!(
                "chains must be 1, 2 or 4, got {}",
                self.chains
            )));
        }
        if self.lines == 0 || self.lines as usize > PAGE / LINE {
            return Err(KnobError(format!(
                "lines must be 1..={}, got {}",
                PAGE / LINE,
                self.lines
            )));
        }
        if self.sparse_pages == 0 || self.sparse_pages > buffers.sparse_capacity() {
            return Err(KnobError(format!(
                "sparse_pages must be 1..={}, got {}",
                buffers.sparse_capacity(),
                self.sparse_pages
            )));
        }
        if self.code_pages == 0 || self.code_pages > buffers.code_capacity() {
            return Err(KnobError(format!(
                "code_pages must be 1..={}, got {}",
                buffers.code_capacity(),
                self.code_pages
            )));
        }
        if !(self.codevisits >= 0.0 && self.codevisits.is_finite()) {
            return Err(KnobError(format!(
                "codevisits must be finite and non-negative, got {}",
                self.codevisits
            )));
        }
        if self.period_ms == 0 {
            return Err(KnobError("period_ms must be positive".into()));
        }
        Ok(())
    }
}

/// Per-second duty factor: lognormal with mean 1 and the given CV,
/// seeded from the run seed and the wall-clock second so every
/// thread draws the same value. A CV of zero is exactly 1.
pub fn duty_factor(seed: u64, second: u64, cv: f64) -> f64 {
    if cv <= 0.0 {
        return 1.0;
    }
    let dist = LogNormal::from_mean_cv(1.0, cv).expect("finite cv");
    let mut rng =
        Xoshiro256PlusPlus::seed_from_u64(seed ^ second.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    dist.sample(&mut rng)
}

/// Executable pages, one tiny stub each at a page-dependent offset.
#[derive(Debug)]
pub struct CodePages {
    buf: AnonBuffer,
    pages: usize,
}

/// `lea rax, [rdi+1]; ret`
#[cfg(target_arch = "x86_64")]
const STUB: &[u8] = &[0x48, 0x8d, 0x47, 0x01, 0xc3];

impl CodePages {
    /// Offset of the stub within page `i`, spread over cache sets.
    pub fn offset(i: usize) -> usize {
        (i * 160) % (PAGE - 64)
    }

    /// Write stubs into `pages` fresh pages and make them executable.
    #[cfg(target_arch = "x86_64")]
    pub fn new(pages: usize) -> std::io::Result<CodePages> {
        let buf = AnonBuffer::new(pages * PAGE, false)?;
        for i in 0..pages {
            let dst = buf.as_ptr().wrapping_add(i * PAGE + Self::offset(i));
            // SAFETY: inside the mapping, which is writable until mprotect.
            unsafe { std::ptr::copy_nonoverlapping(STUB.as_ptr(), dst, STUB.len()) };
        }
        let ptr = std::ptr::NonNull::new(buf.as_ptr().cast()).expect("mapped");
        // SAFETY: the whole mapping, now holding only stubs.
        unsafe {
            mprotect(
                ptr,
                pages * PAGE,
                ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
            )
        }?;
        Ok(CodePages { buf, pages })
    }

    /// Not implemented on this architecture.
    #[cfg(not(target_arch = "x86_64"))]
    pub fn new(_pages: usize) -> std::io::Result<CodePages> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "code stubs are only implemented for x86_64",
        ))
    }

    /// Number of pages.
    pub fn len(&self) -> usize {
        self.pages
    }

    /// Whether there are no pages; there never are.
    pub fn is_empty(&self) -> bool {
        self.pages == 0
    }

    /// Call the stub on page `i` with `x`; it returns `x + 1`.
    #[inline]
    pub fn call(&self, i: usize, x: u64) -> u64 {
        let addr = self.buf.as_ptr().wrapping_add(i * PAGE + Self::offset(i));
        // SAFETY: `addr` is a stub written by `new` in a page that is
        // now executable, with the C ABI the stub implements.
        let f: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(addr) };
        f(x)
    }
}

/// Everything the streams touch. Capacities are fixed; knobs choose
/// how much of each to use.
#[derive(Debug)]
pub struct Buffers {
    main: AnonBuffer,
    sparse: AnonBuffer,
    code: CodePages,
}

impl Buffers {
    /// Allocate `main_bytes` of main buffer, `sparse_pages` pages for
    /// the sparse stream and `code_pages` executable pages.
    pub fn new(
        main_bytes: usize,
        huge: bool,
        sparse_pages: usize,
        code_pages: usize,
    ) -> std::io::Result<Buffers> {
        Ok(Buffers {
            main: AnonBuffer::new(main_bytes.max(PAGE), huge)?,
            sparse: AnonBuffer::new(sparse_pages.max(1) * PAGE, false)?,
            code: CodePages::new(code_pages.max(1))?,
        })
    }

    /// The main buffer.
    pub fn main(&self) -> &AnonBuffer {
        &self.main
    }

    /// Pages available to the sparse stream.
    pub fn sparse_capacity(&self) -> usize {
        self.sparse.len() / PAGE
    }

    /// Pages available to the code stream.
    pub fn code_capacity(&self) -> usize {
        self.code.len()
    }
}

/// Counters a worker keeps, readable while it runs.
#[derive(Debug, Default)]
pub struct WorkerStats {
    /// Accesses completed.
    pub accesses: AtomicU64,
    /// Periods completed.
    pub periods: AtomicU64,
    /// Nanoseconds spent in the busy part of periods.
    pub busy_ns: AtomicU64,
    /// Thread id, once known.
    pub tid: AtomicU64,
}

/// Non-temporal store of one line at `p`.
#[inline(always)]
fn nt_store_line(p: *mut u8, v: u64) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `p` is 8-byte aligned inside a mapped line; SSE2 is
    // baseline on x86_64.
    unsafe {
        for k in 0..(LINE / 8) {
            std::arch::x86_64::_mm_stream_si64(p.add(k * 8).cast::<i64>(), v as i64);
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    // SAFETY: as above, with plain stores.
    unsafe {
        for k in 0..(LINE / 8) {
            std::ptr::write_volatile(p.add(k * 8).cast::<u64>(), v);
        }
    }
}

/// `ops` iterations over `chains` independent ALU chains.
#[inline(always)]
fn compute(ops: u32, chains: u8, s: &mut [u64; 4]) {
    const K: u64 = 0x2545_F491_4F6C_DD1D;
    let [mut a, mut b, mut c, mut d] = *s;
    match chains {
        1 => {
            for _ in 0..ops {
                a = a.wrapping_mul(K).wrapping_add(1);
                // SAFETY: an empty asm block that only pins a register.
                unsafe {
                    std::arch::asm!("/* {0} */", inout(reg) a, options(nomem, nostack, preserves_flags))
                };
            }
        }
        2 => {
            for _ in 0..ops {
                a = a.wrapping_mul(K).wrapping_add(1);
                b ^= b << 13;
                // SAFETY: as above.
                unsafe {
                    std::arch::asm!("/* {0} {1} */", inout(reg) a, inout(reg) b, options(nomem, nostack, preserves_flags))
                };
            }
        }
        _ => {
            for _ in 0..ops {
                a = a.wrapping_add(b);
                b ^= c;
                c = c.wrapping_add(d);
                d ^= a;
                // SAFETY: as above.
                unsafe {
                    std::arch::asm!("/* {0} {1} {2} {3} */", inout(reg) a, inout(reg) b, inout(reg) c, inout(reg) d, options(nomem, nostack, preserves_flags))
                };
            }
        }
    }
    *s = [a, b, c, d];
}

struct Worker {
    buffers: Arc<Buffers>,
    knobs: Arc<RwLock<Knobs>>,
    stop: Arc<AtomicBool>,
    stats: Arc<WorkerStats>,
    seed: u64,
    index: usize,
}

impl Worker {
    /// One access: the streams, then compute.
    #[inline(always)]
    fn access(
        &self,
        k: &Knobs,
        rng: &mut Xoshiro256PlusPlus,
        state: &mut [u64; 4],
        code_acc: &mut f64,
    ) {
        let main = self.buffers.main.as_ptr();
        let main_pages = self.buffers.main.len() / PAGE;
        let lines = k.lines as usize;
        let page = rng.random_range(0..main_pages);
        let line0 = rng.random_range(0..=(PAGE / LINE - lines));
        let stores = (k.wfrac * lines as f64).round() as usize;
        for i in 0..lines {
            let p = main.wrapping_add(page * PAGE + (line0 + i) * LINE);
            // SAFETY: inside the main buffer.
            unsafe {
                let v = std::ptr::read_volatile(p.cast::<u64>());
                state[0] ^= v;
                if i < stores {
                    std::ptr::write_volatile(p.cast::<u64>(), v.wrapping_add(1));
                }
            }
        }
        for _ in 0..k.ntlines {
            let page = rng.random_range(0..main_pages);
            let line = rng.random_range(0..PAGE / LINE);
            nt_store_line(main.wrapping_add(page * PAGE + line * LINE), state[1]);
        }
        let sparse = self.buffers.sparse.as_ptr();
        for _ in 0..k.tlbvisits {
            let page = rng.random_range(0..k.sparse_pages);
            let p = sparse.wrapping_add(page * PAGE + (page % (PAGE / LINE)) * LINE);
            // SAFETY: inside the sparse buffer.
            state[2] ^= unsafe { std::ptr::read_volatile(p.cast::<u64>()) };
        }
        *code_acc += k.codevisits;
        while *code_acc >= 1.0 {
            let page = rng.random_range(0..k.code_pages);
            state[3] = self.buffers.code.call(page, state[3]);
            *code_acc -= 1.0;
        }
        compute(k.ops, k.chains, state);
    }

    fn run(self, cpu: Option<u32>) {
        self.stats
            .tid
            .store(nix::unistd::gettid().as_raw() as u64, Ordering::Relaxed);
        if let Some(cpu) = cpu {
            let mut set = CpuSet::new();
            if set.set(cpu as usize).is_ok() {
                let _ = sched_setaffinity(Pid::from_raw(0), &set);
            }
        }
        let mut rng =
            Xoshiro256PlusPlus::seed_from_u64(self.seed.wrapping_add(self.index as u64 + 1));
        let mut state = [self.seed | 1, 2, 3, 4];
        let mut code_acc = 0.0;
        let mut last_second = u64::MAX;
        let mut factor = 1.0;
        while !self.stop.load(Ordering::Relaxed) {
            let k = self.knobs.read().expect("knobs lock").clone();
            let period = Duration::from_millis(k.period_ms);
            let start = Instant::now();
            let second = crate::record::unix_now() as u64;
            if second != last_second {
                factor = duty_factor(self.seed, second, k.cv);
                last_second = second;
            }
            let duty = (k.duty * factor).min(1.0);
            let busy_until = start + period.mul_f64(duty);
            let mut accesses = 0u64;
            while Instant::now() < busy_until {
                for _ in 0..BATCH {
                    self.access(&k, &mut rng, &mut state, &mut code_acc);
                }
                accesses += u64::from(BATCH);
            }
            let busy = start.elapsed();
            self.stats.accesses.fetch_add(accesses, Ordering::Relaxed);
            self.stats
                .busy_ns
                .fetch_add(busy.as_nanos() as u64, Ordering::Relaxed);
            self.stats.periods.fetch_add(1, Ordering::Relaxed);
            if busy < period {
                std::thread::sleep(period - busy);
            }
        }
        std::hint::black_box(state);
    }
}

/// Running workers.
#[derive(Debug)]
pub struct Generator {
    knobs: Arc<RwLock<Knobs>>,
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    stats: Vec<Arc<WorkerStats>>,
    cpus: Vec<Option<u32>>,
}

impl Generator {
    /// Start `threads` workers. With `cpus`, worker `i` is pinned to
    /// `cpus[i % len]`.
    pub fn start(
        buffers: Arc<Buffers>,
        knobs: Knobs,
        threads: usize,
        cpus: Option<&[u32]>,
        seed: u64,
    ) -> Result<Generator, KnobError> {
        knobs.validate(&buffers)?;
        if threads == 0 {
            return Err(KnobError("threads must be at least 1".into()));
        }
        if cpus.is_some_and(|c| c.is_empty()) {
            return Err(KnobError("an empty CPU list cannot pin anything".into()));
        }
        let knobs = Arc::new(RwLock::new(knobs));
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(threads);
        let mut stats = Vec::with_capacity(threads);
        let mut pinned = Vec::with_capacity(threads);
        for index in 0..threads {
            let cpu = cpus.map(|c| c[index % c.len()]);
            let st = Arc::new(WorkerStats::default());
            let w = Worker {
                buffers: Arc::clone(&buffers),
                knobs: Arc::clone(&knobs),
                stop: Arc::clone(&stop),
                stats: Arc::clone(&st),
                seed,
                index,
            };
            handles.push(
                std::thread::Builder::new()
                    .name(format!("worker{index}"))
                    .spawn(move || w.run(cpu))
                    .map_err(|e| KnobError(e.to_string()))?,
            );
            stats.push(st);
            pinned.push(cpu);
        }
        Ok(Generator {
            knobs,
            stop,
            handles,
            stats,
            cpus: pinned,
        })
    }

    /// Current knobs.
    pub fn knobs(&self) -> Knobs {
        self.knobs.read().expect("knobs lock").clone()
    }

    /// Replace the knobs; workers pick them up at their next period.
    pub fn set_knobs(&self, knobs: Knobs, buffers: &Buffers) -> Result<(), KnobError> {
        knobs.validate(buffers)?;
        *self.knobs.write().expect("knobs lock") = knobs;
        Ok(())
    }

    /// Per-worker counters.
    pub fn stats(&self) -> &[Arc<WorkerStats>] {
        &self.stats
    }

    /// Number of workers.
    pub fn threads(&self) -> usize {
        self.handles.len()
    }

    /// CPUs the workers are pinned to, if any; one entry per worker.
    pub fn pinned_cpus(&self) -> Option<Vec<u32>> {
        self.cpus.iter().copied().collect()
    }

    /// Stop and join the workers.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for Generator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_buffers() -> Arc<Buffers> {
        Arc::new(Buffers::new(4 << 20, false, 64, 16).unwrap())
    }

    #[test]
    fn duty_factor_has_mean_one_and_the_requested_cv() {
        assert_eq!(duty_factor(1, 2, 0.0), 1.0);
        assert_eq!(
            duty_factor(7, 99, 0.5),
            duty_factor(7, 99, 0.5),
            "deterministic"
        );
        assert_ne!(duty_factor(7, 99, 0.5), duty_factor(7, 100, 0.5));
        assert_ne!(duty_factor(7, 99, 0.5), duty_factor(8, 99, 0.5));
        for cv in [0.1, 0.3, 0.73, 1.5] {
            let n = 200_000u64;
            let xs: Vec<f64> = (0..n).map(|s| duty_factor(42, s, cv)).collect();
            let mean = xs.iter().sum::<f64>() / n as f64;
            let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
            let got_cv = var.sqrt() / mean;
            assert!((mean - 1.0).abs() < 0.02, "cv {cv}: mean {mean}");
            assert!(
                (got_cv - cv).abs() < 0.05 * cv.max(0.3),
                "cv {cv}: got {got_cv}"
            );
            assert!(xs.iter().all(|x| *x > 0.0));
        }
    }

    #[test]
    fn knob_validation() {
        let b = small_buffers();
        let ok = Knobs {
            sparse_pages: 64,
            code_pages: 16,
            ..Knobs::default()
        };
        ok.validate(&b).unwrap();
        let bad = |f: fn(&mut Knobs)| {
            let mut k = ok.clone();
            f(&mut k);
            k.validate(&b).unwrap_err()
        };
        assert!(bad(|k| k.duty = 1.5).0.contains("duty"));
        assert!(bad(|k| k.wfrac = -0.1).0.contains("wfrac"));
        assert!(bad(|k| k.cv = f64::NAN).0.contains("cv"));
        assert!(bad(|k| k.chains = 3).0.contains("chains"));
        assert!(bad(|k| k.lines = 0).0.contains("lines"));
        assert!(bad(|k| k.lines = 65).0.contains("lines"));
        assert!(bad(|k| k.sparse_pages = 65).0.contains("sparse_pages"));
        assert!(bad(|k| k.sparse_pages = 0).0.contains("sparse_pages"));
        assert!(bad(|k| k.code_pages = 17).0.contains("code_pages"));
        assert!(bad(|k| k.codevisits = -1.0).0.contains("codevisits"));
        assert!(bad(|k| k.period_ms = 0).0.contains("period"));
        assert_eq!(b.sparse_capacity(), 64);
        assert_eq!(b.code_capacity(), 16);
        assert_eq!(b.main().len(), 4 << 20);
        assert_eq!(KnobError("x".into()).to_string(), "x");
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn code_stubs_return_x_plus_one() {
        let c = CodePages::new(8).unwrap();
        assert_eq!(c.len(), 8);
        assert!(!c.is_empty());
        for i in 0..8 {
            assert_eq!(c.call(i, 41 + i as u64), 42 + i as u64);
        }
        assert_ne!(CodePages::offset(1), CodePages::offset(2));
        assert!(CodePages::offset(1000) + STUB.len() <= PAGE);
    }

    #[test]
    fn compute_chains_change_state() {
        for chains in [1u8, 2, 4] {
            let mut s = [1, 2, 3, 4];
            compute(100, chains, &mut s);
            assert_ne!(s, [1, 2, 3, 4], "chains {chains}");
        }
    }

    #[test]
    fn workers_honour_duty_and_retuning() {
        let b = small_buffers();
        let knobs = Knobs {
            duty: 0.5,
            sparse_pages: 64,
            code_pages: 16,
            tlbvisits: 2,
            codevisits: 0.5,
            ntlines: 1,
            ..Knobs::default()
        };
        let busy_ns = |g: &Generator| {
            g.stats()
                .iter()
                .map(|s| s.busy_ns.load(Ordering::Relaxed))
                .sum::<u64>()
        };
        let start = Instant::now();
        let g = Generator::start(Arc::clone(&b), knobs.clone(), 2, None, 1).unwrap();
        assert_eq!(g.threads(), 2);
        assert_eq!(g.pinned_cpus(), None);
        std::thread::sleep(Duration::from_millis(600));
        let elapsed = start.elapsed().as_secs_f64();
        let cores = busy_ns(&g) as f64 / 1e9 / elapsed;
        assert!(
            (0.7..=1.3).contains(&cores),
            "2 threads at duty 0.5 should use about 1 core, used {cores}"
        );
        let accesses: u64 = g
            .stats()
            .iter()
            .map(|s| s.accesses.load(Ordering::Relaxed))
            .sum();
        let periods: u64 = g
            .stats()
            .iter()
            .map(|s| s.periods.load(Ordering::Relaxed))
            .sum();
        assert!(accesses > 1000, "{accesses}");
        assert!(periods >= 40, "{periods}");
        for s in g.stats() {
            let busy_frac = s.busy_ns.load(Ordering::Relaxed) as f64
                / (s.periods.load(Ordering::Relaxed) as f64 * 10e6);
            assert!(
                (0.4..=0.7).contains(&busy_frac),
                "busy fraction {busy_frac}"
            );
            assert!(s.tid.load(Ordering::Relaxed) > 0);
        }
        assert_eq!(g.knobs(), knobs);

        g.set_knobs(
            Knobs {
                duty: 0.0,
                ..knobs.clone()
            },
            &b,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let idle_before = busy_ns(&g);
        std::thread::sleep(Duration::from_millis(300));
        let idle = (busy_ns(&g) - idle_before) as f64 / 1e9;
        assert!(idle < 0.02, "duty 0 should be nearly idle, used {idle} s");
        assert!(g.set_knobs(Knobs { chains: 3, ..knobs }, &b).is_err());
        g.stop();
    }

    #[test]
    fn pinning_and_start_errors() {
        let b = small_buffers();
        let knobs = Knobs {
            duty: 0.1,
            sparse_pages: 1,
            code_pages: 1,
            ..Knobs::default()
        };
        assert!(Generator::start(Arc::clone(&b), knobs.clone(), 0, None, 1).is_err());
        assert!(Generator::start(Arc::clone(&b), knobs.clone(), 1, Some(&[]), 1).is_err());
        assert!(
            Generator::start(
                Arc::clone(&b),
                Knobs {
                    lines: 0,
                    ..knobs.clone()
                },
                1,
                None,
                1
            )
            .is_err()
        );
        let g = Generator::start(Arc::clone(&b), knobs, 3, Some(&[0, 1]), 1).unwrap();
        assert_eq!(g.pinned_cpus(), Some(vec![0, 1, 0]));
        std::thread::sleep(Duration::from_millis(50));
        let tid = g.stats()[1].tid.load(Ordering::Relaxed);
        let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")).unwrap();
        let allowed = status
            .lines()
            .find(|l| l.starts_with("Cpus_allowed_list:"))
            .unwrap();
        assert!(
            allowed.ends_with("\t1") || allowed.ends_with(" 1"),
            "{allowed}"
        );
        drop(g);
    }
}
