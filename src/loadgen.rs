//! The load generator: worker threads that repeat one *access* inside
//! a duty-cycled period. `docs/design.md` section 5 maps each knob to
//! the observable it moves.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::sched::{CpuSet, sched_setaffinity};
use nix::sys::mman::{ProtFlags, mprotect};
use nix::unistd::Pid;
use rand::SeedableRng;
use rand::seq::SliceRandom;
use rand_xoshiro::Xoshiro256PlusPlus;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::footprint::{AnonBuffer, FileMap};

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
    /// Independent dependency chains in the compute loop, 0.25 to 4; a
    /// fraction mixes the two nearest of 1, 2 and 4 within each access,
    /// and below one a share of the iterations runs a long-latency
    /// dependent chain instead, so compute IPC can fall below the
    /// single-chain floor.
    pub chains: f64,
    /// Consecutive lines touched per main-stream visit, up to 64; a
    /// fraction alternates between the two nearest counts, and below
    /// one it is the share of accesses that visit at all.
    pub lines: f64,
    /// Share of main-stream visits that continue where the worker's
    /// last one ended instead of jumping to a random page, 0 to 1;
    /// each worker walks its own slice of the working set.
    #[serde(default)]
    pub seq: f64,
    /// Share of the random visits whose page is computed from the
    /// value loaded just before it, and whose further lines each land
    /// in a page of their own, each waiting for the line before, so no
    /// miss can issue until the previous one completes, 0 to 1.
    #[serde(default)]
    pub dep: f64,
    /// Fraction of the main buffer, from its start, that the main and
    /// non-temporal streams touch, above 0 and at most 1.
    #[serde(default = "one")]
    pub wset: f64,
    /// Fraction of main-stream lines that are stored to, 0 to 1.
    pub wfrac: f64,
    /// Lines written with non-temporal stores per access; may be below one.
    pub ntlines: f64,
    /// Sparse-stream loads per access; may be below one.
    pub tlbvisits: f64,
    /// Pages the sparse stream picks from; at most the buffer's capacity.
    pub sparse_pages: usize,
    /// Stub calls per access; may be below one.
    pub codevisits: f64,
    /// Executable pages the code stream picks from; at most the capacity.
    pub code_pages: usize,
    /// Page-cache reads per access: `pread` of one page of the page-cache
    /// file, at a random page, so the kernel copies it; may be below one.
    #[serde(default)]
    pub kreads: f64,
    /// Period length.
    pub period_ms: u64,
}

fn one() -> f64 {
    1.0
}

impl Default for Knobs {
    fn default() -> Self {
        Knobs {
            duty: 1.0,
            cv: 0.0,
            ops: 64,
            chains: 2.0,
            lines: 4.0,
            seq: 0.0,
            dep: 0.0,
            wset: 1.0,
            wfrac: 0.25,
            ntlines: 0.0,
            tlbvisits: 0.0,
            sparse_pages: 1,
            codevisits: 0.0,
            code_pages: 1,
            kreads: 0.0,
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
        unit(self.seq, "seq")?;
        unit(self.dep, "dep")?;
        if !(self.wset > 0.0 && self.wset <= 1.0) {
            return Err(KnobError(format!(
                "wset must be above 0 and at most 1, got {}",
                self.wset
            )));
        }
        if !(self.cv >= 0.0 && self.cv.is_finite()) {
            return Err(KnobError(format!(
                "cv must be finite and non-negative, got {}",
                self.cv
            )));
        }
        if !(SLOWEST_CHAINS..=4.0).contains(&self.chains) {
            return Err(KnobError(format!(
                "chains must be in {SLOWEST_CHAINS}..=4, got {}",
                self.chains
            )));
        }
        if !(self.lines > 0.0 && self.lines <= (PAGE / LINE) as f64) {
            return Err(KnobError(format!(
                "lines must be above 0 and at most {}, got {}",
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
        if self.kreads > 0.0 && !buffers.has_cache() {
            return Err(KnobError(
                "kreads needs a page-cache file to read".to_string(),
            ));
        }
        for (name, v) in [
            ("codevisits", self.codevisits),
            ("tlbvisits", self.tlbvisits),
            ("kreads", self.kreads),
            ("ntlines", self.ntlines),
        ] {
            if !(v >= 0.0 && v.is_finite()) {
                return Err(KnobError(format!(
                    "{name} must be finite and non-negative, got {v}"
                )));
            }
        }
        if self.period_ms == 0 {
            return Err(KnobError("period_ms must be positive".into()));
        }
        Ok(())
    }
}

/// Factors in a block. Within a block the factors are the block's
/// midpoint quantiles of the lognormal, in an order seeded from the
/// block, so every block has exactly the set's mean and CV and a
/// recording a few blocks long reads them off, where independent
/// draws needed `(cv / tolerance)²` seconds to.
pub const FACTOR_BLOCK: u64 = 20;
/// Seconds a factor holds. A recorder whose one-second samples are
/// not aligned to the generator's seconds averages two factors in the
/// samples that straddle a change, which lowers the CV it sees by a
/// third of the variance in those samples; one change in four seconds
/// keeps that below the tolerance where one a second lost a fifth of
/// the CV on average and a third at the worst phase.
pub const FACTOR_SECONDS: u64 = 4;

/// The `i`th of a block's factors: the lognormal with mean 1 and CV
/// `cv` at probability (i + ½) / block.
pub fn factor_quantile(cv: f64, i: u64) -> f64 {
    let sigma2 = (1.0 + cv * cv).ln();
    let p = (i as f64 + 0.5) / FACTOR_BLOCK as f64;
    let z = std::f64::consts::SQRT_2 * puruspe::inverf(2.0 * p - 1.0);
    (-0.5 * sigma2 + sigma2.sqrt() * z).exp()
}

/// Duty factor of a wall-clock second: the block's quantile at this
/// second's slot, held for [`FACTOR_SECONDS`], of a permutation seeded
/// from the run seed and the block, so every thread uses the same
/// value and every block the whole set. A CV of zero is exactly 1.
pub fn duty_factor(seed: u64, second: u64, cv: f64) -> f64 {
    if cv <= 0.0 {
        return 1.0;
    }
    let plateau = second / FACTOR_SECONDS;
    let block = plateau / FACTOR_BLOCK;
    let slot = (plateau % FACTOR_BLOCK) as usize;
    let mut order: [u8; FACTOR_BLOCK as usize] = std::array::from_fn(|i| i as u8);
    let mut rng =
        Xoshiro256PlusPlus::seed_from_u64(seed ^ block.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    order.shuffle(&mut rng);
    factor_quantile(cv, u64::from(order[slot]))
}

/// Mean and coefficient of variation of the busy fraction a worker
/// actually runs at `duty` and `cv` over a block: the per-second
/// factors clipped at full duty, so the mean falls short of `duty`
/// and the CV of the knob once the clip bites. Exactly the set the
/// workers run, so the fitter can set both knobs from the model and
/// the generator knows what each second intends.
pub fn clipped_duty_stats(duty: f64, cv: f64) -> (f64, f64) {
    if cv <= 0.0 {
        return (duty.min(1.0), 0.0);
    }
    let xs: Vec<f64> = (0..FACTOR_BLOCK)
        .map(|i| (duty * factor_quantile(cv, i)).min(1.0))
        .collect();
    let n = FACTOR_BLOCK as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    (mean, if mean > 0.0 { var.sqrt() / mean } else { 0.0 })
}

/// Executable pages, one tiny stub each at a page-dependent offset.
/// Stub pages are spaced [`CODE_STRIDE`] pages apart in virtual memory
/// and the gaps are never touched, so no 16 KiB block is fully mapped
/// and AMD's TLB cannot coalesce neighbouring pages into one entry.
#[derive(Debug)]
pub struct CodePages {
    buf: AnonBuffer,
    pages: usize,
}

/// Virtual pages between consecutive stub pages.
pub const CODE_STRIDE: usize = 4;

/// `lea rax, [rdi+1]; ret`
#[cfg(target_arch = "x86_64")]
const STUB: &[u8] = &[0x48, 0x8d, 0x47, 0x01, 0xc3];
/// `add x0, x0, #1; ret`, little-endian.
#[cfg(target_arch = "aarch64")]
const STUB: &[u8] = &[0x00, 0x04, 0x00, 0x91, 0xc0, 0x03, 0x5f, 0xd6];

/// Make stubs written through the data side visible to the instruction
/// side. x86 keeps its instruction cache coherent with stores; `AArch64`
/// does not: each stub's line is cleaned to the point of unification,
/// the instruction cache is invalidated for it, and an `isb` follows,
/// as the architecture requires before executing written code. Each
/// stub lies within one line, since offsets are multiples of 32.
#[cfg(target_arch = "x86_64")]
fn sync_instructions(_stubs: &[*const u8]) {}
#[cfg(target_arch = "aarch64")]
fn sync_instructions(stubs: &[*const u8]) {
    // SAFETY: cache maintenance on addresses inside a mapping of our own.
    unsafe {
        for &p in stubs {
            std::arch::asm!("dc cvau, {0}", in(reg) p, options(nostack, preserves_flags));
        }
        std::arch::asm!("dsb ish", options(nostack, preserves_flags));
        for &p in stubs {
            std::arch::asm!("ic ivau, {0}", in(reg) p, options(nostack, preserves_flags));
        }
        std::arch::asm!("dsb ish", "isb", options(nostack, preserves_flags));
    }
}

impl CodePages {
    /// Offset of the stub within page `i`, spread over cache sets.
    pub fn offset(i: usize) -> usize {
        (i * 160) % (PAGE - 64)
    }

    /// Write stubs into `pages` fresh pages and make them executable.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub fn new(pages: usize) -> std::io::Result<CodePages> {
        let buf = AnonBuffer::new_untouched(pages * CODE_STRIDE * PAGE, false)?;
        let mut stubs = Vec::with_capacity(pages);
        for i in 0..pages {
            let dst = buf
                .as_ptr()
                .wrapping_add(i * CODE_STRIDE * PAGE + Self::offset(i));
            // SAFETY: inside the mapping, which is writable until mprotect.
            unsafe { std::ptr::copy_nonoverlapping(STUB.as_ptr(), dst, STUB.len()) };
            stubs.push(dst.cast_const());
        }
        sync_instructions(&stubs);
        let ptr = std::ptr::NonNull::new(buf.as_ptr().cast()).expect("mapped");
        // SAFETY: the whole mapping, now holding only stubs.
        unsafe {
            mprotect(
                ptr,
                pages * CODE_STRIDE * PAGE,
                ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
            )
        }?;
        Ok(CodePages { buf, pages })
    }

    /// Not implemented on this architecture.
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    pub fn new(_pages: usize) -> std::io::Result<CodePages> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "code stubs are only implemented for x86_64 and aarch64",
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
        let addr = self
            .buf
            .as_ptr()
            .wrapping_add(i * CODE_STRIDE * PAGE + Self::offset(i));
        // SAFETY: `addr` is a stub written by `new` in a page that is
        // now executable, with the C ABI the stub implements.
        let f: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(addr) };
        f(x)
    }
}

/// The pages the sparse stream picks from: anonymous memory, or a
/// mapping of the page-cache file when the anonymous footprint cannot
/// spare them and the profile has page cache to lend.
#[derive(Debug)]
pub enum SparseSet {
    /// Anonymous pages of the process's own.
    Anon(AnonBuffer),
    /// Pages of the page-cache file, read only.
    File(FileMap),
}

impl SparseSet {
    fn as_ptr(&self) -> *const u8 {
        match self {
            SparseSet::Anon(b) => b.as_ptr(),
            SparseSet::File(m) => m.as_ptr(),
        }
    }

    fn len(&self) -> usize {
        match self {
            SparseSet::Anon(b) => b.len(),
            SparseSet::File(m) => m.len(),
        }
    }
}

/// Everything the streams touch. Capacities are fixed; knobs choose
/// how much of each to use.
#[derive(Debug)]
pub struct Buffers {
    main: AnonBuffer,
    sparse: SparseSet,
    code: CodePages,
    cache: Option<CacheRead>,
}

/// The page-cache file as the kernel reads it for the page-cache
/// reads: its descriptor and its length in pages.
#[derive(Debug)]
pub struct CacheRead {
    fd: OwnedFd,
    pages: usize,
}

impl CacheRead {
    /// A reader of `file`, which must be the page-cache file.
    pub fn new(file: &File) -> std::io::Result<CacheRead> {
        let pages = (file.metadata()?.len() / PAGE as u64) as usize;
        if pages == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the page-cache file holds no whole page",
            ));
        }
        Ok(CacheRead {
            fd: file.try_clone()?.into(),
            pages,
        })
    }
}

impl Buffers {
    /// Allocate `main_bytes` of main buffer, `sparse_pages` anonymous
    /// pages for the sparse stream and `code_pages` executable pages.
    pub fn new(
        main_bytes: usize,
        huge: bool,
        sparse_pages: usize,
        code_pages: usize,
    ) -> std::io::Result<Buffers> {
        Self::with_sparse(
            main_bytes,
            huge,
            SparseSet::Anon(AnonBuffer::new(sparse_pages.max(1) * PAGE, false)?),
            code_pages,
            None,
        )
    }

    /// As [`new`](Self::new), with the sparse set given.
    pub fn with_sparse(
        main_bytes: usize,
        huge: bool,
        sparse: SparseSet,
        code_pages: usize,
        cache: Option<CacheRead>,
    ) -> std::io::Result<Buffers> {
        Ok(Buffers {
            main: AnonBuffer::new(main_bytes.max(PAGE), huge)?,
            sparse,
            code: CodePages::new(code_pages.max(1))?,
            cache,
        })
    }

    /// Whether page-cache reads have a file to read.
    pub fn has_cache(&self) -> bool {
        self.cache.is_some()
    }

    /// The main buffer.
    pub fn main(&self) -> &AnonBuffer {
        &self.main
    }

    /// Whether the sparse stream runs on the page-cache file.
    pub fn sparse_on_file(&self) -> bool {
        matches!(self.sparse, SparseSet::File(_))
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
    /// Page-cache reads completed.
    pub kreads: AtomicU64,
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
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as above; `stnp` writes a non-temporal pair of 8 bytes at
    // each 16-byte step of the line.
    unsafe {
        for k in 0..(LINE / 16) {
            std::arch::asm!(
                "stnp {v}, {v}, [{p}]",
                v = in(reg) v,
                p = in(reg) p.add(k * 16),
                options(nostack, preserves_flags)
            );
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
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
                    std::arch::asm!("/* {0} */", inout(reg) a, options(nomem, nostack, preserves_flags));
                };
            }
        }
        2 => {
            for _ in 0..ops {
                a = a.wrapping_mul(K).wrapping_add(1);
                b ^= b << 13;
                // SAFETY: as above.
                unsafe {
                    std::arch::asm!("/* {0} {1} */", inout(reg) a, inout(reg) b, options(nomem, nostack, preserves_flags));
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
                    std::arch::asm!("/* {0} {1} {2} {3} */", inout(reg) a, inout(reg) b, inout(reg) c, inout(reg) d, options(nomem, nostack, preserves_flags));
                };
            }
        }
    }
    *s = [a, b, c, d];
}

/// Lowest `chains` value: every iteration on the slow chain.
pub const SLOWEST_CHAINS: f64 = 0.25;

/// `ops` iterations of one dependent chain through a floating-point
/// square root and back, some thirty cycles for a handful of
/// instructions, for compute IPC well below what a multiply chain
/// gives. Integer division was faster than its reputation here, since
/// a small divisor makes the divider quick.
#[inline(always)]
fn compute_slow(ops: u32, s: &mut [u64; 4]) {
    let mut a = s[0] | 1;
    for _ in 0..ops {
        let f = ((a >> 1) | 1) as f64;
        a ^= (f.sqrt() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        // SAFETY: an empty asm block that only pins a register.
        unsafe {
            std::arch::asm!("/* {0} */", inout(reg) a, options(nomem, nostack, preserves_flags));
        };
    }
    s[0] = a;
}

/// `ops` iterations split between the two chain counts nearest to
/// `chains`, so the knob is continuous: 1.5 runs half the iterations
/// with one chain and half with two; 0.5 runs a third of them on the
/// slow chain. Worked out once per period, not per access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ComputeSplit {
    lo: (u32, u8),
    hi: (u32, u8),
    slow: u32,
}

impl ComputeSplit {
    fn of(ops: u32, chains: f64) -> ComputeSplit {
        if chains < 1.0 {
            let slow_frac = ((1.0 - chains) / (1.0 - SLOWEST_CHAINS)).clamp(0.0, 1.0);
            let slow = (f64::from(ops) * slow_frac).round() as u32;
            return ComputeSplit {
                lo: (ops - slow, 1),
                hi: (0, 2),
                slow,
            };
        }
        let (lo, hi, frac) = if chains <= 2.0 {
            (1u8, 2u8, chains - 1.0)
        } else {
            (2u8, 4u8, (chains - 2.0) / 2.0)
        };
        let hi_ops = (f64::from(ops) * frac.clamp(0.0, 1.0)).round() as u32;
        ComputeSplit {
            lo: (ops - hi_ops, lo),
            hi: (hi_ops, hi),
            slow: 0,
        }
    }

    #[inline(always)]
    fn run(&self, s: &mut [u64; 4]) {
        compute(self.lo.0, self.lo.1, s);
        if self.hi.0 > 0 {
            compute(self.hi.0, self.hi.1, s);
        }
        if self.slow > 0 {
            compute_slow(self.slow, s);
        }
    }
}

#[cfg(test)]
fn compute_mixed(ops: u32, chains: f64, s: &mut [u64; 4]) {
    ComputeSplit::of(ops, chains).run(s);
}

/// The streams' random source: `SplitMix64`, one word of state that
/// stays in a register, a few instructions a draw.
#[derive(Clone, Copy, Debug)]
struct Draws(u64);

impl Draws {
    fn seeded(seed: u64) -> Draws {
        Draws(seed)
    }

    #[inline(always)]
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// `x` scaled into `0..n` from a uniform 32-bit value: a multiply and
/// a shift, where a modulus would be a division per access.
#[inline(always)]
fn scale32(x: u32, n: usize) -> usize {
    ((u64::from(x) * n as u64) >> 32) as usize
}

/// What the knobs mean for the hot path, worked out once per period
/// so an access does no division, rounding or float-to-int work.
#[derive(Debug)]
struct Pace {
    pages: usize,
    slice_start: usize,
    slice_lines: usize,
    lines_whole: usize,
    lines_fract: f64,
    /// Stores for `lines_whole` lines and for one more.
    stores: [usize; 2],
    compute: ComputeSplit,
    /// Accesses between clock checks: a batch of cheap accesses, or a
    /// single one heavy with compute, which would otherwise overrun
    /// the busy part of the period.
    batch: u32,
    /// Pages the sparse walk advances per visit.
    sparse_stride: usize,
}

/// A stride coprime to the page count, near the golden ratio of it, so
/// the sparse walk visits every page once per cycle in an order no
/// next-page prefetcher follows. A cycle longer than the TLB misses on
/// every visit, where random visits to the same pages hit whenever the
/// draw repeats one still cached: on a 72-entry L1 dTLB, 128 random
/// pages missed 44 percent of the time.
fn sparse_stride(pages: usize) -> usize {
    if pages <= 2 {
        return 1;
    }
    let mut stride = ((pages as f64 * 0.618_033_988_75).round() as usize).clamp(1, pages - 1);
    while gcd(stride, pages) != 1 {
        stride += 1;
    }
    stride
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl Pace {
    fn new(k: &Knobs, w: &Worker) -> Pace {
        let (pages, slice_start, slice_pages) = w.geometry(k.wset);
        let whole = k.lines as usize;
        Pace {
            pages,
            slice_start,
            slice_lines: slice_pages * PAGE_LINES,
            lines_whole: whole,
            lines_fract: k.lines - whole as f64,
            stores: [
                (k.wfrac * whole as f64).round() as usize,
                (k.wfrac * (whole + 1) as f64).round() as usize,
            ],
            compute: ComputeSplit::of(k.ops, k.chains),
            batch: if k.ops >= 256 { 1 } else { BATCH },
            sparse_stride: sparse_stride(k.sparse_pages),
        }
    }
}

struct Worker {
    buffers: Arc<Buffers>,
    knobs: Arc<RwLock<Knobs>>,
    stop: Arc<AtomicBool>,
    stats: Arc<WorkerStats>,
    seed: u64,
    index: usize,
    threads: usize,
}

/// What a worker carries from one access to the next: the fractions
/// of rates below one per access, so they are honoured over many
/// accesses, the walks' positions, and the page buffer.
#[derive(Debug, Default)]
struct Carry {
    lines: f64,
    nt: f64,
    tlb: f64,
    code: f64,
    seq: f64,
    dep: f64,
    /// Line index of the sequential walk within the worker's slice.
    cursor: usize,
    /// Page the sparse walk is at.
    sparse_at: usize,
    kread: f64,
    /// Where the kernel copies a page-cache page to; a page, once a
    /// read has happened.
    pagebuf: Vec<u8>,
}

/// Lines per page.
const PAGE_LINES: usize = PAGE / LINE;

impl Worker {
    /// Pages of the working set and this worker's slice of it, as
    /// (pages, slice start, slice length), from the main buffer and
    /// `wset`. Slices are equal, one per worker, never beyond the set.
    fn geometry(&self, wset: f64) -> (usize, usize, usize) {
        let main_pages = self.buffers.main.len() / PAGE;
        let pages = ((main_pages as f64 * wset) as usize).clamp(1, main_pages);
        let slice = (pages / self.threads.max(1)).max(1);
        let start = (self.index % pages.div_ceil(slice)) * slice;
        (pages, start, slice.min(pages - start))
    }

    /// One access: the streams, then compute. `chase` is the value the
    /// last main-stream line loaded, passed by value and returned so it
    /// stays in a register and reaches memory only on a dependent visit.
    #[inline(always)]
    fn access(
        &self,
        k: &Knobs,
        p: &Pace,
        rng: &mut Draws,
        state: &mut [u64; 4],
        c: &mut Carry,
        mut chase: u64,
    ) -> u64 {
        let main = self.buffers.main.as_ptr();
        c.lines += p.lines_fract;
        let extra = usize::from(c.lines >= 1.0);
        if extra == 1 {
            c.lines -= 1.0;
        }
        let lines = p.lines_whole + extra;
        let mut dependent = false;
        if lines > 0 {
            c.seq += k.seq;
            let (page, line0) = if c.seq >= 1.0 {
                // Walk the slice line by line, never across a page within
                // one visit, wrapping at the slice end.
                c.seq -= 1.0;
                if c.cursor % PAGE_LINES + lines > PAGE_LINES {
                    c.cursor = (c.cursor / PAGE_LINES + 1) * PAGE_LINES;
                }
                if c.cursor >= p.slice_lines {
                    c.cursor = 0;
                }
                let at = (p.slice_start + c.cursor / PAGE_LINES, c.cursor % PAGE_LINES);
                c.cursor += lines;
                at
            } else {
                c.dep += k.dep;
                let mut draw = rng.next();
                if c.dep >= 1.0 {
                    // The XOR makes the address depend on the previous
                    // load without changing its distribution. The barrier
                    // keeps this a branch: folded into a conditional
                    // move, every visit would carry the dependency.
                    c.dep -= 1.0;
                    dependent = true;
                    draw ^= std::hint::black_box(chase);
                }
                // The offset leaves room for the longer of the two run
                // lengths so the address depends on the draw alone. Made
                // to depend on the carry, which lives in memory, it waited
                // on a store that could not commit before the previous
                // miss returned, and consecutive misses stopped
                // overlapping: 123 ns an access against 47 on Zen 4.
                (
                    scale32((draw >> 32) as u32, p.pages),
                    scale32(draw as u32, PAGE_LINES - p.lines_whole),
                )
            };
            let stores = p.stores[extra];
            for i in 0..lines {
                let at = if dependent && i > 0 {
                    // Each further line of a dependent visit lands in a page
                    // of its own and waits for the line before: the term is
                    // zero, but with both operands opaque neither the
                    // compiler nor the core can know that until the load
                    // returns. A shift or mask the compiler could see
                    // through was folded away. Scattered over one page
                    // instead, the region prefetcher fetched the lot.
                    let d = rng.next();
                    scale32((d >> 32) as u32, p.pages) * PAGE
                        + scale32(d as u32, PAGE_LINES) * LINE
                        + (std::hint::black_box(chase) & std::hint::black_box(0u64)) as usize
                } else {
                    page * PAGE + (line0 + i) * LINE
                };
                let q = main.wrapping_add(at);
                // SAFETY: inside the main buffer.
                unsafe {
                    let v = std::ptr::read_volatile(q.cast::<u64>());
                    chase = v;
                    state[0] ^= v;
                    if i < stores {
                        std::ptr::write_volatile(q.cast::<u64>(), v.wrapping_add(1));
                    }
                }
            }
        }
        c.nt += k.ntlines;
        while c.nt >= 1.0 {
            let draw = rng.next();
            let page = scale32((draw >> 32) as u32, p.pages);
            let line = scale32(draw as u32, PAGE_LINES);
            nt_store_line(main.wrapping_add(page * PAGE + line * LINE), state[1]);
            c.nt -= 1.0;
        }
        let sparse = self.buffers.sparse.as_ptr();
        c.tlb += k.tlbvisits;
        while c.tlb >= 1.0 {
            // The stride is below the page count, so one subtraction
            // wraps, except just after the set shrank under the cursor.
            let mut page = c.sparse_at + p.sparse_stride;
            if page >= k.sparse_pages {
                page -= k.sparse_pages;
                if page >= k.sparse_pages {
                    page %= k.sparse_pages;
                }
            }
            c.sparse_at = page;
            let q = sparse.wrapping_add(page * PAGE + (page % PAGE_LINES) * LINE);
            // SAFETY: inside the sparse buffer.
            state[2] ^= unsafe { std::ptr::read_volatile(q.cast::<u64>()) };
            c.tlb -= 1.0;
        }
        c.code += k.codevisits;
        while c.code >= 1.0 {
            let page = scale32((rng.next() >> 32) as u32, k.code_pages);
            state[3] = self.buffers.code.call(page, state[3]);
            c.code -= 1.0;
        }
        if let Some(cache) = &self.buffers.cache {
            c.kread += k.kreads;
            if c.kread >= 1.0 && c.pagebuf.is_empty() {
                c.pagebuf = vec![0; PAGE];
            }
            while c.kread >= 1.0 {
                let page = scale32((rng.next() >> 32) as u32, cache.pages);
                // The kernel copies the page out of the page cache into the
                // worker's buffer; a byte of it is folded in.
                if let Ok(n) =
                    nix::sys::uio::pread(&cache.fd, &mut c.pagebuf[..], (page * PAGE) as i64)
                {
                    state[3] ^= u64::from(c.pagebuf[n.saturating_sub(1)]);
                    self.stats.kreads.fetch_add(1, Ordering::Relaxed);
                }
                c.kread -= 1.0;
            }
        }
        p.compute.run(state);
        chase
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
        let mut rng = Draws::seeded(self.seed.wrapping_add(self.index as u64 + 1));
        let mut state = [self.seed | 1, 2, 3, 4];
        let mut carry = Carry::default();
        let mut chase = 0u64;
        let mut last_second = u64::MAX;
        let mut factor = 1.0;
        while !self.stop.load(Ordering::Relaxed) {
            let k = self.knobs.read().expect("knobs lock").clone();
            let pace = Pace::new(&k, &self);
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
                for _ in 0..pace.batch {
                    chase = self.access(&k, &pace, &mut rng, &mut state, &mut carry, chase);
                }
                accesses += u64::from(pace.batch);
            }
            let busy = start.elapsed();
            self.stats.accesses.fetch_add(accesses, Ordering::Relaxed);
            self.stats
                .busy_ns
                .fetch_add(busy.as_nanos() as u64, Ordering::Relaxed);
            self.stats.periods.fetch_add(1, Ordering::Relaxed);
            if busy < period {
                std::thread::sleep(period.saturating_sub(busy));
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
        if cpus.is_some_and(<[u32]>::is_empty) {
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
                threads,
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
    fn duty_factor_runs_every_block_through_the_quantile_set() {
        assert_eq!(duty_factor(1, 2, 0.0), 1.0);
        assert_eq!(
            duty_factor(7, 99, 0.5),
            duty_factor(7, 99, 0.5),
            "deterministic"
        );
        assert_ne!(duty_factor(7, 99, 0.5), duty_factor(7, 100, 0.5));
        for s in 0..FACTOR_SECONDS {
            assert_eq!(
                duty_factor(7, 40 + s, 0.5),
                duty_factor(7, 40, 0.5),
                "a factor holds for its seconds"
            );
        }
        let block = |seed: u64, b: u64, cv: f64| -> Vec<f64> {
            (0..FACTOR_BLOCK)
                .map(|slot| duty_factor(seed, (b * FACTOR_BLOCK + slot) * FACTOR_SECONDS, cv))
                .collect()
        };
        let sorted = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v
        };
        let mut last_cv = 0.0;
        for cv in [0.1, 0.3, 0.73, 1.5] {
            let set: Vec<f64> = (0..FACTOR_BLOCK).map(|i| factor_quantile(cv, i)).collect();
            assert!(set.iter().all(|x| *x > 0.0));
            for b in [0u64, 1, 5_000_000] {
                assert_eq!(
                    sorted(block(42, b, cv)),
                    sorted(set.clone()),
                    "block {b} cv {cv}"
                );
            }
            assert_ne!(block(42, 0, cv), block(42, 1, cv), "blocks differ in order");
            assert_ne!(block(42, 0, cv), block(43, 0, cv), "seeds differ in order");
            // The model is the clipped set itself.
            let (mean, got_cv) = clipped_duty_stats(0.5, cv);
            let n = FACTOR_BLOCK as f64;
            let clipped: Vec<f64> = set.iter().map(|x| (0.5 * x).min(1.0)).collect();
            let m = clipped.iter().sum::<f64>() / n;
            let v = clipped.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n;
            assert!((mean - m).abs() < 1e-12 && (got_cv - v.sqrt() / m).abs() < 1e-12);
            let raw = set.iter().sum::<f64>() / n;
            assert!((raw - 1.0).abs() < 0.15, "cv {cv}: set mean {raw}");
            assert!(
                got_cv > last_cv,
                "cv {cv}: set CV {got_cv} rises with the knob"
            );
            last_cv = got_cv;
        }
    }

    #[test]
    fn clipped_duty_statistics() {
        assert_eq!(clipped_duty_stats(0.3, 0.0), (0.3, 0.0));
        let (m, c) = clipped_duty_stats(0.3, 0.5);
        assert!(
            (m - 0.3).abs() < 0.02 && (c - 0.5).abs() < 0.05,
            "{m} {c}: barely clipped"
        );
        let (m, c) = clipped_duty_stats(0.9, 0.8);
        assert!(m < 0.85 && c < 0.5, "{m} {c}: the clip bites");
        assert_eq!(clipped_duty_stats(0.9, 0.8), (m, c), "deterministic");
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
        assert!(bad(|k| k.chains = 0.2).0.contains("chains"));
        assert!(bad(|k| k.chains = 4.5).0.contains("chains"));
        Knobs {
            chains: 0.25,
            ..ok.clone()
        }
        .validate(&b)
        .unwrap();
        assert!(bad(|k| k.seq = 1.1).0.contains("seq"));
        assert!(bad(|k| k.dep = -0.5).0.contains("dep"));
        assert!(bad(|k| k.wset = 0.0).0.contains("wset"));
        assert!(bad(|k| k.wset = 1.5).0.contains("wset"));
        assert!(bad(|k| k.wset = f64::NAN).0.contains("wset"));
        assert!(bad(|k| k.lines = 0.0).0.contains("lines"));
        assert!(bad(|k| k.lines = 65.0).0.contains("lines"));
        Knobs {
            lines: 1.0 / 1024.0,
            ..ok.clone()
        }
        .validate(&b)
        .unwrap();
        assert!(bad(|k| k.lines = f64::NAN).0.contains("lines"));
        assert!(bad(|k| k.sparse_pages = 65).0.contains("sparse_pages"));
        assert!(bad(|k| k.sparse_pages = 0).0.contains("sparse_pages"));
        assert!(bad(|k| k.code_pages = 17).0.contains("code_pages"));
        assert!(bad(|k| k.codevisits = -1.0).0.contains("codevisits"));
        assert!(bad(|k| k.tlbvisits = f64::INFINITY).0.contains("tlbvisits"));
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
        assert_eq!(
            c.buf.len(),
            8 * CODE_STRIDE * PAGE,
            "stride reserved, gaps untouched"
        );
    }

    #[test]
    fn compute_chains_change_state() {
        for chains in [1u8, 2, 4] {
            let mut s = [1, 2, 3, 4];
            compute(100, chains, &mut s);
            assert_ne!(s, [1, 2, 3, 4], "chains {chains}");
        }
        for chains in [0.25, 0.5, 1.0, 1.5, 2.0, 3.0, 4.0] {
            let mut s = [1, 2, 3, 4];
            compute_mixed(100, chains, &mut s);
            assert_ne!(s, [1, 2, 3, 4], "chains {chains}");
        }
        let mut slow = [1, 2, 3, 4];
        compute_slow(100, &mut slow);
        assert_ne!(slow[0], 1);
        let mut a = [1, 2, 3, 4];
        compute_mixed(100, 1.0, &mut a);
        let mut b = [1, 2, 3, 4];
        compute(100, 1, &mut b);
        assert_eq!(a, b, "an integer count is exactly that loop");
    }

    #[test]
    fn workers_honour_duty_and_retuning() {
        let b = small_buffers();
        let knobs = Knobs {
            duty: 0.5,
            sparse_pages: 64,
            code_pages: 16,
            tlbvisits: 2.5,
            codevisits: 0.5,
            ntlines: 1.5,
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
        assert!(
            g.set_knobs(
                Knobs {
                    chains: 9.0,
                    ..knobs
                },
                &b
            )
            .is_err()
        );
        g.stop();
    }

    /// A worker on `b` with no thread running, for driving `access`
    /// by hand.
    fn bench_worker(b: &Arc<Buffers>, index: usize, threads: usize, knobs: &Knobs) -> Worker {
        Worker {
            buffers: Arc::clone(b),
            knobs: Arc::new(RwLock::new(knobs.clone())),
            stop: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(WorkerStats::default()),
            seed: 1,
            index,
            threads,
        }
    }

    /// First word of every line of the main buffer.
    fn line_words(b: &Buffers) -> Vec<u64> {
        (0..b.main().len() / LINE)
            .map(|i| {
                // SAFETY: inside the main buffer, 8-byte aligned.
                unsafe { std::ptr::read_volatile(b.main().as_ptr().add(i * LINE).cast::<u64>()) }
            })
            .collect()
    }

    /// Lines whose first word differs between two snapshots: the ones
    /// stored to in between.
    fn changed(before: &[u64], b: &Buffers) -> Vec<usize> {
        before
            .iter()
            .zip(line_words(b))
            .enumerate()
            .filter(|(_, (x, y))| **x != *y)
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn sequential_visits_walk_the_workers_slice_without_crossing_pages() {
        // 1024 pages; wset 0.5 keeps 512; 4 workers get 128 each; worker 1
        // starts at page 128.
        let b = small_buffers();
        let knobs = Knobs {
            seq: 1.0,
            lines: 3.0,
            wfrac: 1.0,
            wset: 0.5,
            ops: 1,
            ..Knobs::default()
        };
        let w = bench_worker(&b, 1, 4, &knobs);
        assert_eq!(w.geometry(0.5), (512, 128, 128));
        assert_eq!(w.geometry(1.0), (1024, 256, 256));
        assert_eq!(bench_worker(&b, 3, 4, &knobs).geometry(0.001), (1, 0, 1));
        let pace = Pace::new(&knobs, &w);
        assert_eq!(
            (pace.pages, pace.slice_start, pace.slice_lines, pace.stores),
            (512, 128, 128 * 64, [3, 4])
        );
        assert_eq!(
            bench_worker(&b, 5, 8, &knobs).geometry(3.0 / 1024.0),
            (3, 2, 1),
            "more workers than pages share pages"
        );
        let before = line_words(&b);
        let pace = Pace::new(&knobs, &w);
        let mut rng = Draws::seeded(1);
        let mut state = [1u64, 2, 3, 4];
        let mut c = Carry::default();
        let mut chase = 0u64;
        for _ in 0..22 {
            chase = w.access(&knobs, &pace, &mut rng, &mut state, &mut c, chase);
        }
        // 21 visits of 3 lines fill lines 0..63 of page 128; the 22nd would
        // straddle, so it moved to page 129.
        let lines = changed(&before, &b);
        assert_eq!(lines.len(), 66);
        assert_eq!(lines[0], 128 * PAGE_LINES);
        assert!(!lines.contains(&(128 * PAGE_LINES + 63)), "line 63 skipped");
        assert_eq!(lines[63], 129 * PAGE_LINES);
        assert_eq!(c.cursor, PAGE_LINES + 3);
        // The walk wraps at the slice end: 128 pages of 21 visits each.
        for _ in 22..=(128 * 21) {
            chase = w.access(&knobs, &pace, &mut rng, &mut state, &mut c, chase);
        }
        assert_eq!(c.cursor, 3, "back at the slice start");
        let lines = changed(&before, &b);
        assert!(
            lines
                .iter()
                .all(|l| (128 * PAGE_LINES..256 * PAGE_LINES).contains(l))
        );
        assert_eq!(lines.len(), 128 * 63);
    }

    #[test]
    fn random_visits_stay_in_the_working_set_and_dependent_ones_follow_the_load() {
        let b = small_buffers();
        let knobs = Knobs {
            lines: 1.0,
            wfrac: 1.0,
            wset: 0.25,
            dep: 0.5,
            ops: 1,
            ..Knobs::default()
        };
        let w = bench_worker(&b, 0, 1, &knobs);
        let before = line_words(&b);
        let pace = Pace::new(&knobs, &w);
        let mut rng = Draws::seeded(7);
        let mut state = [1u64, 2, 3, 4];
        let mut c = Carry::default();
        let mut chase = 0u64;
        for _ in 0..20_000 {
            chase = w.access(&knobs, &pace, &mut rng, &mut state, &mut c, chase);
        }
        let lines = changed(&before, &b);
        assert!(lines.len() > 10_000, "{}", lines.len());
        assert!(
            lines.iter().all(|l| *l < 256 * PAGE_LINES),
            "nothing beyond a quarter of the buffer"
        );
        assert!(
            lines.iter().any(|l| *l > 200 * PAGE_LINES),
            "spread over the set"
        );
        // Every other visit was dependent: its page came from the draw
        // XOR the value loaded before, which the stores had made non-zero.
        assert!(chase != 0, "the last load saw a stored value");
        assert!(
            c.dep.abs() < 1e-9,
            "10000 dependent visits consumed the carry"
        );
    }

    #[test]
    fn dependent_visits_spread_their_lines_over_pages() {
        let b = small_buffers();
        let knobs = Knobs {
            lines: 4.0,
            wfrac: 1.0,
            dep: 1.0,
            ops: 1,
            ..Knobs::default()
        };
        let w = bench_worker(&b, 0, 1, &knobs);
        let before = line_words(&b);
        let pace = Pace::new(&knobs, &w);
        let mut rng = Draws::seeded(5);
        let mut state = [1u64, 2, 3, 4];
        let mut c = Carry::default();
        let mut chase = 0u64;
        for _ in 0..200 {
            chase = w.access(&knobs, &pace, &mut rng, &mut state, &mut c, chase);
        }
        let lines = changed(&before, &b);
        let pages: std::collections::BTreeSet<usize> =
            lines.iter().map(|l| l / PAGE_LINES).collect();
        // Up to 800 lines over 1024 pages: far more pages than the 200
        // visits, and next to no runs of adjacent lines.
        assert!(pages.len() > 450, "{} pages", pages.len());
        assert!(
            lines.len() > 600 && lines.len() <= 800,
            "{} lines",
            lines.len()
        );
        let runs = lines
            .windows(2)
            .filter(|w| w[1] == w[0] + 1 && w[1] / PAGE_LINES == w[0] / PAGE_LINES)
            .count();
        assert!(runs < lines.len() / 8, "{runs} adjacent pairs: not a run");
    }

    #[test]
    fn a_run_length_below_one_visits_on_that_share_of_accesses() {
        let b = small_buffers();
        let knobs = Knobs {
            lines: 0.25,
            wfrac: 1.0,
            ops: 1,
            ..Knobs::default()
        };
        let w = bench_worker(&b, 0, 1, &knobs);
        let before = line_words(&b);
        let pace = Pace::new(&knobs, &w);
        assert_eq!(
            (pace.lines_whole, pace.lines_fract, pace.stores),
            (0, 0.25, [0, 1])
        );
        let mut rng = Draws::seeded(3);
        let mut state = [1u64, 2, 3, 4];
        let mut c = Carry::default();
        let mut chase = 0u64;
        for _ in 0..40 {
            chase = w.access(&knobs, &pace, &mut rng, &mut state, &mut c, chase);
        }
        assert_eq!(
            changed(&before, &b).len(),
            10,
            "one line every fourth access"
        );
        assert_eq!(scale32(u32::MAX, 10), 9);
        let mut d = Draws::seeded(1);
        let xs: Vec<u64> = (0..4).map(|_| d.next()).collect();
        assert!(xs.windows(2).all(|w| w[0] != w[1]));
        assert_eq!(Draws::seeded(1).next(), xs[0], "deterministic");
        assert_eq!(scale32(0, 10), 0);
        assert_eq!(scale32(1 << 31, 10), 5);
        assert_eq!(
            ComputeSplit::of(100, 0.5),
            ComputeSplit {
                lo: (33, 1),
                hi: (0, 2),
                slow: 67
            }
        );
        assert_eq!(
            ComputeSplit::of(100, 3.0),
            ComputeSplit {
                lo: (50, 2),
                hi: (50, 4),
                slow: 0
            }
        );
        assert_eq!(
            ComputeSplit::of(7, 1.0),
            ComputeSplit {
                lo: (7, 1),
                hi: (0, 2),
                slow: 0
            }
        );
    }

    #[test]
    fn the_sparse_stream_can_run_on_a_page_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = crate::footprint::CacheFile::create(dir.path(), 64 * PAGE as u64).unwrap();
        let map = cache.map(64 * PAGE).unwrap();
        let b =
            Arc::new(Buffers::with_sparse(1 << 20, false, SparseSet::File(map), 1, None).unwrap());
        assert!(b.sparse_on_file());
        assert_eq!(b.sparse_capacity(), 64);
        let knobs = Knobs {
            duty: 0.5,
            tlbvisits: 3.0,
            sparse_pages: 64,
            code_pages: 1,
            ..Knobs::default()
        };
        let g = Generator::start(Arc::clone(&b), knobs, 1, None, 1).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let accesses = g.stats()[0].accesses.load(Ordering::Relaxed);
        assert!(
            accesses > 100,
            "visits over the file's pages ran: {accesses}"
        );
        g.stop();
    }

    #[test]
    fn page_cache_reads_go_through_the_kernel() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = crate::footprint::CacheFile::create(dir.path(), 64 * PAGE as u64).unwrap();
        cache.settle().unwrap();
        let reader = CacheRead::new(cache.file()).unwrap();
        assert_eq!(reader.pages, 64);
        let b = Arc::new(
            Buffers::with_sparse(
                1 << 20,
                false,
                SparseSet::Anon(AnonBuffer::new(PAGE, false).unwrap()),
                1,
                Some(reader),
            )
            .unwrap(),
        );
        assert!(b.has_cache());
        let knobs = Knobs {
            duty: 0.5,
            kreads: 2.5,
            sparse_pages: 1,
            code_pages: 1,
            ..Knobs::default()
        };
        knobs.validate(&b).unwrap();
        let g = Generator::start(Arc::clone(&b), knobs, 1, None, 1).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let s = &g.stats()[0];
        let (accesses, kreads) = (
            s.accesses.load(Ordering::Relaxed),
            s.kreads.load(Ordering::Relaxed),
        );
        g.stop();
        assert!(accesses > 20, "{accesses}");
        let per_access = kreads as f64 / accesses as f64;
        assert!(
            (2.3..=2.7).contains(&per_access),
            "{kreads} reads over {accesses} accesses"
        );
        let without = small_buffers();
        assert!(!without.has_cache());
        let bad = Knobs {
            kreads: 1.0,
            sparse_pages: 1,
            code_pages: 1,
            ..Knobs::default()
        };
        assert!(bad.validate(&without).unwrap_err().0.contains("kreads"));
    }

    #[test]
    fn heavy_accesses_still_honour_the_duty_cycle() {
        let b = small_buffers();
        let knobs = Knobs {
            duty: 0.3,
            ops: 200_000,
            chains: 1.0,
            lines: 1.0,
            sparse_pages: 1,
            code_pages: 1,
            ..Knobs::default()
        };
        let g = Generator::start(Arc::clone(&b), knobs, 1, None, 1).unwrap();
        std::thread::sleep(Duration::from_millis(700));
        let s = &g.stats()[0];
        let busy_frac = s.busy_ns.load(Ordering::Relaxed) as f64
            / (s.periods.load(Ordering::Relaxed) as f64 * 10e6);
        assert!(
            (0.2..=0.5).contains(&busy_frac),
            "busy fraction {busy_frac} at duty 0.3 with heavy accesses"
        );
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
                    lines: 0.0,
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
