# Design

## 1. Contract

`run` takes a profile, a flat map from observable to target value, and
produces load whose observables, measured on its own scope the way the
recorder measures them, approach those targets. It reports, per target:
the value asked for, the value measured in every window, the error, and
the tier the measurement came from (section 4).

Three rules bound what "approach" means:

- **Directional on aggregates.** A profile built from percentiles mixes
  containers, so its targets need not be jointly reachable. The fitter
  does best effort on all of them and prints the residuals. It never
  rejects a profile for being inconsistent.
- **Scope is per observable.** Busy cores and memory are scoped to a
  cgroup or process by construction. Counters are scoped per task or
  per cgroup when perf allows and host-wide otherwise. DRAM bandwidth
  is host-wide unless a resctrl monitoring group exists. Every sample
  and every profile field carries the scope it was measured at.
- **Degrade, never fail.** A missing privilege or counter moves an
  observable to the next tier and labels it. The only fatal errors are
  bad input and the inability to allocate the requested footprint.

What is reproduced is the set of numbers below. Access-pattern
locality, kernel-mode work, I/O, sub-second timing and anything else
outside the set are not claimed, and the report does not imply them.

## 2. Observables

All rates are counter deltas over the sampling interval. Ratios are
ratios of sums over the window, never means of per-sample ratios.
"cgroup" means the cgroup v2 files under the scope's directory,
"process" means the task and its threads, "self" means the running
generator.

| key | unit | role | recorded from | self-measured by |
| --- | --- | --- | --- | --- |
| `cpu_limit_cores` | cores | hint, thread count | cgroup `cpu.max` quota / period; absent when `max` | the cgroup `run` is in, if any |
| `memory_limit_bytes` | bytes | hint, not enforced; `run` notes when the memory targets exceed it | cgroup `memory.max`; absent when `max` | same |
| `cpu_usage_cores` | cores | target | cgroup `cpu.stat` `usage_usec`; process `/proc/<pid>/stat` utime+stime over all threads; host `/proc/stat` non-idle | `getrusage(RUSAGE_SELF)` utime+stime, or the cgroup file when alone in one |
| `cpu_usage_cv_1s` | none | target | 1 Hz samples of the above, pooled over windows (section 3) | same |
| `instructions_per_sec` | 1/s | consistency | perf `instructions` | perf, own threads |
| `cycles_per_sec` | 1/s | consistency | perf `cycles` | same |
| `ipc` | none | target | instructions / cycles | same |
| `l3_misses_per_sec` | 1/s | consistency | vendor event, section 2.1 | same, or the AMD proxy |
| `l3_mpki` | per 1000 instr | target | L3 misses × 1000 / instructions | same |
| `dram_bytes_per_sec` | bytes/s | target | resctrl `mbm_total_bytes` of the scope's monitoring group; host-wide memory-controller PMUs when privileged and no group | resctrl, else modelled from counted fills and bytes written non-temporally |
| `dtlb_pki` | per 1000 instr | target | vendor event, section 2.1 | perf, own threads |
| `itlb_pki` | per 1000 instr | target | vendor event, section 2.1 | same |
| `memory_current_bytes` | bytes | target | cgroup `memory.current`; process: sum of the three below | `/proc/self/status` plus file residency |
| `anon_bytes` | bytes | target | cgroup `memory.stat` `anon`; process `RssAnon` | `RssAnon` |
| `pagecache_bytes` | bytes | target | cgroup `memory.stat` `file` minus `shmem`; process `RssFile`, which only counts mapped pages and is labelled so | `cachestat(2)` on the generator's own file |
| `shmem_bytes` | bytes | target | cgroup `memory.stat` `shmem`; process `RssShmem` | `RssShmem` |
| `tlb_host_ipc`, `tlb_host_minutes` | none, minutes | provenance | host IPC and sample time behind host-scoped TLB fields | not applicable |

Kernel memory is `memory_current_bytes` minus the three components; it
is reproduced by creating empty files, whose dentry and inode slab is
charged to the generator's cgroup, at an estimated 1100 bytes each to
start, and their count then follows what the measured total stands
above or below the target, damped and once a second. The generator
measures its memory and busy time from its own cgroup when it has one
to itself, as in a container or a scope of its own, since the cgroup
charges the slab, the page tables and the threads' stacks that the
process view does not see, and is what a recorder of the scope reads;
twelve instances on one host ran a fifth over their memory target
before that, and the process view could neither see it nor correct
it. A cgroup shared with other processes, a login session's scope
say, would count theirs too, so there the process view stays.

Each field in a profile carries provenance: the scope (`cgroup`,
`process`, `host`), the counting mode (`user`, `all`) and the method
(`perf`, `cgroup`, `proc`, `resctrl`, `proxy`, `model`).

### 2.1 Vendor events

Vendors count different things, so a profile is comparable only on its
own vendor's definitions. The tables live in `src/events.rs` with
fixtures copied from the kernel's `tools/perf/pmu-events` JSON at v7.1,
or, for Apple's cores, which the kernel has no table for, from the
driver `drivers/perf/apple_m1_cpu_pmu.c` and a sweep of every event
number on an M2 Ultra.

| field | AMD Zen 3, 4, 5 | Intel Skylake-SP and later server cores | ARM Neoverse | Apple M2 |
| --- | --- | --- | --- | --- |
| L3 misses | L3 PMU lookup-miss event, raw `amd_l3/event=0x04,umask=0x01/` on Zen 4 and 5 and `event=0x06,umask=0x01` on Zen 3, per L3 domain, filtered to the scope's cores with the `coreid` / `threadmask` fields. Unprivileged proxy: core event `ls_any_fills_from_sys.dram_io_all`, raw `event=0x44,umask=0x48`, data cache fills from DRAM or MMIO | `LONGEST_LAT_CACHE.MISS`, `event=0x2e,umask=0x41` | `LL_CACHE_MISS_RD`, `0x37`, reads only | P cores: event `0x23`, in no list, which the sweep found counting one per 128-byte line a demand load or store fetched from memory: 1.00 per visit of a random 2 GiB set with 16 KiB pages and with 32 MiB hugepages alike, 2.00 with a store to a second line, 0 for a 32 MiB set the system cache holds, 0 for a prefetched sequential read, so like AMD's proxy it does not count prefetched lines. E cores: none; their work adds instructions and cycles but no misses, and the plan's note says so |
| dTLB | L1 dTLB misses that hit the L2 TLB: raw `event=0x45,umask=0x0f` | `DTLB_LOAD_MISSES.WALK_COMPLETED` plus `DTLB_STORE_MISSES.WALK_COMPLETED`; `0x08` and `0x49` with umask `0x0e` through Ice Lake-SP and on Sierra Forest, `0x12` and `0x13` from Sapphire Rapids | `DTLB_WALK`, `0x34` | `L2_TLB_MISS_DATA`, `0x0b`: an access that missed the last TLB level, 0.98 per visit of a set past it on both classes of core, where the walk event `MMU_TABLE_WALK_DATA`, `0x08`, counted 1.0 on the P cores and 1.5 on the E |
| iTLB | L2 iTLB misses: raw `event=0x85,umask=0x0f` | `ITLB_MISSES.WALK_COMPLETED`; `0x85` or `0x11`, umask `0x0e`, split as above | `ITLB_WALK`, `0x35` | `L1I_TLB_MISS_DEMAND`, `0xd4`: a demand instruction fetch that missed the first-level iTLB, 1.2 per call into a stub page of a set past the reach, steady at a call on one access in two hundred. The L2 miss and walk events, `0x0a` and `0x07`, count speculative fetches too: about two per call at a call on every access, but at one call in two hundred bursts of up to one per access, hundreds per call, for seconds at a time, and the L1 fill event `0x04` a dozen per access |

CPU models are mapped to tables the way the kernel's `mapfile.csv`
does it. A model with no verified table gets no counters rather than a
guess, and the report says so; Apple's M1 cores share the driver's
event list with the M2's but not the sweep, so they get none.

A host whose cores come in classes, each with a PMU of its own, as
Apple's P and E cores, opens the same core events on every class's
PMU under one name. A task's event on a class's PMU is enabled for
the task's whole run time but runs only while the task is on that
class's CPUs, so the readings of one name are folded before scaling:
the counts add, the running times add, and the sum is scaled by the
time the task was on no PMU at all, which is multiplexing; scaled one
by one, each would count the time on the other class as if it were.
A per-CPU event opens on the PMU of the CPU's class.

AMD's dTLB definition counts L2 TLB hits where the others count walks;
the two are not comparable, and the profile's vendor field says which
applies. dynolog's hbt library uses the L2-miss definition (umask
`0xf0`) for AMD; this tool follows the definition above because the
reference profiles do.

## 3. Aggregation

`record` writes one sample per interval per scope. `aggregate` builds a
profile from any number of recordings:

- **Container value**: mean of a scope's samples over the recording.
- **Average**: mean of container values, each scope weighted equally,
  so N average containers sum to the fleet.
- **Ratios** (`ipc`, every `_pki`, `l3_mpki`): ratio of summed
  numerators to summed denominators, never a mean of ratios.
- **Percentiles**: over all scopes' samples, not over container values,
  so minute-scale spikes survive. For ratios, over per-sample ratios.
  Linear interpolation between order statistics, as NumPy's default.
- **Sample weight**: a scope's samples are weighted by their actual
  interval length, so a short final interval does not count as much as
  a full one.
- **Burstiness** (`cpu_usage_cv_1s`): for each scope and each window of
  W seconds (default 300), the mean m and variance v of the 1 Hz
  usage samples; then `sqrt(mean v) / mean m` over all windows. Pooled
  this way, N independent average containers carry the fleet's summed
  variance, and idle containers, whose own CV diverges, do not dominate.
  The percentile fields are per-window `sqrt(v) / m`, over windows with
  a positive mean. Variance is the population variance of the window.
  A trailing partial window is kept when it is at least half full, or
  when it is the only one, so a short recording still yields a value.

The profile also records `count`, the number of scopes it was built
from, and the CPU vendor, family and model. A scope is one scope name
in one recording, so the same cgroup recorded twice is two containers.
Recordings from different CPU models, or that measured a field with
different provenance, are refused rather than silently mixed.

## 4. Measurement tiers

At startup `run` and `record` probe every method they might use and
keep the outcome. The table in `docs/privileges.md` gives the fallback
chain per observable. The report labels each target:

- **verified**: measured with the recorder's definition on the tool's
  own scope;
- **proxy**: a different counter known to track it, with the
  calibration ratio shown;
- **modelled**: computed from what the generator knows it emitted;
- **unverified**: run open-loop from a calibration file or the built-in
  default knobs.

The probe result is also what `caps` prints.

Counters are read as the kernel's cumulative counts with their enabled
and running times, and each interval is scaled by the enabled over
the running time gained in that interval, not by the cumulative
ratio: a cumulative ratio belongs to the whole history, and when a
recorder attached to or left the PMU a replay's counters shared, the
share changed and the differenced cumulative readings put IPC at a
hundredth of itself for five seconds and at five times for three.

## 5. Load generator

Each worker thread repeats one *access*: a few memory streams, then
some compute, inside a duty-cycled 10 ms period. Addresses come from a
PRNG, not from loaded data, so misses overlap.

| knob | moves | mechanism |
| --- | --- | --- |
| `threads` | busy cores | default `cpu_limit_cores`, capped at the CPUs in the affinity mask; each worker pinned to one CPU so uncore attribution is possible |
| `duty` | busy cores | busy fraction of each 10 ms period |
| `cv` | burstiness | duty factor held for four wall-clock seconds at a time: within each block of 20 such plateaus, the 20 midpoint quantiles of a lognormal with mean 1 and CV `cv` (σ² = ln(1 + cv²), μ = −σ²/2), in an order seeded from the run seed and the block, so every block carries exactly the set's mean and CV and all threads move together; four seconds, since a recorder whose seconds are not the generator's averages two factors in a sample that straddles a change, which at one change a second lost a fifth of the CV on average |
| `ops`, `chains` | instructions per access, so IPC and every per-instruction rate | `chains` independent ALU dependency chains, 0.25 to 4: a fraction mixes the two nearest of 1, 2 and 4, and below one a share of the iterations runs a long-latency square-root chain; `ops` iterations, kept alive with an empty `asm!` barrier |
| `lines` | L3 misses per access | consecutive lines touched in a random page of the main buffer, 1/1024 to a page's worth, 64 on x86; below one it is the share of accesses that visit at all, so a profile with one miss per hundred thousand instructions needs no hundred thousand ops of compute per access; longer runs let the prefetcher hide latency, raising IPC at equal MPKI. Where the miss event counts demand fetches only and the region prefetcher fetches the rest of a page, Apple's M2, each line of a run lands in a random page of its own instead, independent of the others, since a run of 2, 8 or 128 consecutive, strided or scattered lines in one page counted one miss: so laid out, 2 lines counted 2.0 and, with compute between visits, 8 counted 8.0, where at one op per access 8 counted 1.7 and 64 counted 11, the event undercounting dense back-to-back misses |
| `seq` | dTLB rate down and IPC up at equal MPKI | share of visits that continue where the worker's last one ended, inside the worker's own slice of the working set, never crossing a page within a visit; a walk changes page once per 64 lines and lets the prefetcher stream |
| `dep` | IPC down at equal MPKI | share of the random visits whose page is the PRNG draw XOR the value loaded before it, and whose further lines each land in a page of their own, each waiting for the line before, so no miss can issue until the previous one completes; lines scattered within one page would be fetched together by the region prefetcher |
| `wset` | dTLB rate down at equal MPKI | fraction of the main buffer, from its start, that the main and non-temporal streams touch; a set of hugepage-backed memory within the first-level TLB's reach keeps missing the L3 without TLB events |
| `wfrac` | write share, so DRAM bandwidth at equal misses | fraction of main-stream lines stored to; a dirtied line is written back |
| `ntlines` | DRAM bandwidth beyond L3 misses | full lines written with non-temporal stores: `movnti` on x86-64, `stnp` on AArch64, plain stores elsewhere |
| `tlbvisits`, sparse page count | dTLB rate | one fixed line per page over a page set sized between the L1 and L2 dTLB reach (AMD definition) or beyond the L2 reach (walks), walked in a fixed cycle with a stride coprime to the count, so that every visit to a set past the TLB's reach misses, where a random draw hits whenever it repeats a page still cached (128 random pages over a 72-entry L1 dTLB missed 44 percent of the time), and no next-page prefetcher follows the order; the set is anonymous memory, capped at an eighth of the anonymous target or a quarter of the anonymous field's absolute tolerance, whichever is more, so a tiny process still gets a set past the TLB's reach, or a read-only mapping of the page-cache file when that cap cannot hold it and the file can, so a small process with a large page cache gets its TLB rate from pages that count as page cache |
| `kreads` | kernel-mode misses and instructions, DRAM bandwidth | `pread` of one page of the page-cache file per read, at a random page, into a buffer of the worker's own, so the kernel copies it out of the page cache: what a page-cache reader's kernel time is made of; may be below one per access; a lever only while the file is larger than the L3, since a resident file is copied from cache and a read then moves no DRAM bytes and misses nothing. Measured on Zen 4 over a 1 GiB file on btrfs, a read is 1785 instructions (1664 on tmpfs) and 61 misses with the kernel in the count, and 115 instructions and no misses in user mode alone |
| `codevisits`, code page count | iTLB rate | calls through a function pointer into two-instruction stubs, an add and a return, placed one per executable page at varying offsets, on x86-64 and AArch64, where the stubs' lines are cleaned and the instruction cache invalidated before the pages turn executable; `codevisits` may be below one per access; the page set is anonymous memory under the same cap as the sparse set |
| `huge` | main-stream page walks | `MADV_HUGEPAGE` on the main buffer; the report shows what THP actually backed. Without it the buffer is advised `MADV_NOHUGEPAGE`, since a THP policy of `always` would otherwise back it with hugepages anyway |

Footprint: the main buffer is anonymous memory touched once at start;
page cache is a file of `pagecache_bytes` in a disk-backed scratch
directory, written then read, with residency re-checked and restored;
shmem is a `memfd`; kernel memory is empty files as in section 2.

The streams are laid out on the host's geometry: the page is the
kernel's small page, what a TLB entry covers, 4 KiB on x86 and 16 KiB
on Apple's Linux; the line is what a miss fetches from memory, the
cost of a miss, 64 bytes on x86 and in sysfs's `coherency_line_size`,
128 on Apple's chips, whose L2 and system cache move 128-byte lines
where `CTR_EL0` says 64: a second load 64 bytes into a line the
first had missed on cost an L2 hit and no further fetch (M2 Ultra,
2026-10-18). A run is at most a page of lines, 64 on x86 and 128 on
Apple, and on Apple its lines are in pages of their own, as the
`lines` row says; a page-cache read moves a page; the report's notes
say which geometry a run used. The access path is compiled once per
geometry with the page and line sizes as constants and the host's
picked once a batch: an address made of the sizes as values cost two
instructions a line and nine a sparse visit more, and one of shifts by
a count in a register more still (Zen 4, 2026-10-18); a geometry
without an instantiation of its own runs on the values.

### 5.1 Known limits

The three stream knobs reach the corners the random stream alone could
not: serialized misses for a latency-bound target (`dep`), a prefetched
walk for a bandwidth-bound one (`seq`), and a small hugepage-backed set
for random access without TLB events (`wset`). What stays out of reach:
many misses with almost no dTLB events over 4 KiB pages, which only a
sequential walk produces, so IPC then follows the prefetcher; IPC
below what fully serialized misses give at the target's MPKI; a
set below 64 pages; and, under the L2-hit dTLB definition, fewer
than about 1.5 L1 reloads per hundred hugepage touches, which is
what Zen 4 showed for random visits to anything from 13 to 56
hugepages, so a memory-bound target at low IPC, whose misses must be
dependent and so in pages of their own, keeps a dTLB rate of a few
hundredths per thousand instructions however small its set. A cache-resident target, with almost no misses, is
still met by diluting the rates with compute rather than by a set that
fits the cache, since the miss rate at the cache edge is too steep for
a fitter to stand on. At one op per access the misses per instruction
have a ceiling of their own: each line of the run costs some forty
instructions of bookkeeping on Zen 4, more when it is stored to, so a
profile asking for more than about 22 L3 misses per thousand
instructions at low IPC is past what the run can make, the run length
stops moving MPKI, and the fitter holds and says so rather than
lengthen the run to no effect; stress-ng's 21 sits just under it.
Bandwidth beyond what the counted misses move is out of reach too:
stress-ng's 70 GB/s at 21 MPKI and an IPC of 0.22 is several times
the bytes its demand misses explain, hardware prefetch and streaming
stores the miss proxy does not count, where the generator's bytes
follow its misses, so there the bandwidth rule waits.

On Apple's M2 the limits are the event's. Its miss event counts demand
fetches on the P cores alone, so a profile's misses per instruction
are diluted by whatever ran on the E cores, on the recording and the
replay alike, and a sequential reader or writer, whose lines the
prefetchers bring in, records next to no misses at any bandwidth. At
one op per access the event undercounts dense misses: eight
independent lines in eight pages counted 1.7 where, with sixty-four
ops of compute between visits, they counted 8.0, so the most counted
misses per instruction come with compute between them, and the
fitter's measured elasticities see what the event sees. The L2 TLB
missed none of 2048 random pages and a quarter of 4096, some 3072
entries of 16 KiB, and the main buffer's 32 MiB hugepages fit the
first level, so the main stream makes next to no dTLB events, a few
thousandths per visit, and the rate comes from the sparse set, whose
floor of 4096 pages is 64 MiB here; and there is no bandwidth
measurement without resctrl, so bandwidth is modelled from the
counted misses, which the prefetched traffic is not in.

## 6. Fitting

Knobs depend on the hardware, so they are fit per CPU model and
cached. Order: footprint first and let it settle; threads and duty for
busy cores; chains, then ops and lines for IPC and MPKI; the write share and then
non-temporal lines for bandwidth, only while IPC and MPKI are within
twice their tolerance, since both change them, and non-temporal lines
only for an IPC target of 0.6 or more, since each line is eight stores
retiring at high IPC and 64 of them per access quadrupled IPC and
divided MPKI by ten on Zen 4, where the write share moved neither, and page-cache reads, a page from DRAM each, where non-temporal lines cannot carry the bandwidth or are at their cap, as many as leave MPKI within its band by what a read costs the counts, since in user mode a read is instructions without misses and only dilutes; tlbvisits; codevisits; repeat until
every verified target is within tolerance or the pass limit is hit.

Busy cores and burstiness start from the clipped model: the duty and
burstiness knobs are solved so that the block's factors, clipped at
full duty, have the target mean and CV, with the mean given priority
when the duty cannot carry the CV. Each second's busy cores are
compared with what the knobs intended for that second, the duty times
its factor, so the mean's efficiency and the burstiness ratio need no
long-run average, and applied to the clipped model's mean and CV they
give the values a long recording would see; those are what is judged,
reported and corrected by the factor target / predicted. Busy cores
are judged from the third one-second sample, burstiness from the
twelfth, three factors' worth, since its ratio needs seconds that
intended different things. With no intended variation the measured
jitter is the burstiness. A second that intended under a tenth of the
model's mean busy cores carries the process's own overhead and says
nothing of the knobs' efficiency: fio's kernel-mode fit, begun in a
plateau intending a few thousandths of a core, read the overhead as a
23-fold efficiency, cut its duty and spent two budgets of passes
(Zen 4, 2026-10-19); such seconds count toward the burstiness ratio
and not the mean, and until a telling second is in the mean is not
judged. IPC moves by the same factor raised to a
damping exponent of one half, bounded to a quarter and four times per
pass. Line run length answers to IPC; the chain count,
continuous between a quarter and four by mixing the two nearest loops
within an access, with a division chain below one, moves only once the run length is at its range or a pass
past half the range has stopped lifting IPC. The ops rule is
model-based: instructions per access are measured from the counters
and the worker tally, the slope against compute work (ops times
chains) comes from a secant over the last two passes that moved the
work by a quarter or more, the last such secant standing in between,
since over two ops out of fifteen the window noise in instructions
per access gave a slope of two where the large move before had
measured fifteen, and ops swung fourfold every pass; ops is set
to the work that gives the MPKI target at the run length the IPC rule
just chose; a target of no misses at all, which a sequential writer
records where the miss event counts no prefetched line, is aimed at
as half the absolute floor, inside the band, since no finite compute
reaches zero. The per-access rates are then set against the
instructions per access the next pass will have, so a change in ops
never drags them off target: stub calls by their measured events per
visit with the error damped, since they are the only source of iTLB
events; sparse visits by the dTLB events short or over per access at
one per visit, damped, since the set is sized past the reach and the
rest of the events, the main stream's and the non-temporal lines'
pages, do not follow the visit rate, where a ratio moved the events
by a quarter of its step. All per-access rates are fractional, carried over an
accumulator, so low targets are reachable. The burstiness knob moves
once three one-second samples are in and stays within twice its
target, since clipping at full duty keeps the measured value below the
knob; the one-second busy-cores ticks sit on wall-clock
second boundaries, where the workers redraw their factor. Worker
threads are the CPU limit when given, else the busy cores with headroom
of one plus the CV squared, so the clipped per-second factor keeps its
mean: per-second busy cores between zero and the thread count with a
given mean have a CV of at most the all-or-nothing series', and fio
at 0.27 cores with a CV of 3.8 needs five threads, where one plus the
CV gave two and the fit cut duty to no effect; where the burstiness
target is past its floor a twentieth more, so the duty stays below
full and the factors show at all: TaoBench recorded at 2.99 cores got
three threads at full duty and its burstiness fell to the jitter
(Zen 4, 2026-10-19).

The IPC ladder gives the shares back first: the sequential walk when
IPC is high, dependent visits when it is short; a share under a tenth
goes back whole and the ladder continues in the same pass. The order
then depends on the regime. Memory-bound, at one miss per thousand
instructions or more with an IPC target below 1.5, and IPC high:
random visits
are made dependent, by half the relative IPC error per pass between a
twentieth and a fifth, once compute is down to one
chain, at one op, or has stopped moving IPC; then run length falls,
undamped, which is less overlap while the ops rule keeps MPKI; then
chains. Compute-bound with IPC high: shorter runs, fewer chains; with
both at their floors and a visit on only a share of accesses, the
whole access scales up, compute and visit share together, so the slow
chain carries more of the instructions at the same misses per
instruction; dependent visits last. At one op with MPKI short, run
length follows the shortfall by MPKI's measured elasticity to it,
undamped and at most doubling per pass: the first step takes misses
to follow the run one for one, the next knows what the last change of
run length alone did, at one op with the working set and the reads
unchanged. At one op the run's instructions grow with its misses, so
the lever can be spent: eight to sixty-four lines left stress-ng's
MPKI near 17 on Zen 4 where the first step predicted 21; below a tenth
the run length holds and the report says MPKI is at the generator's
ceiling. With the run spent or at its range,
page-cache reads, when the profile's counts cover kernel mode, since
the kernel's copy is where a page-cache reader's misses are; they are
the first lever given back when misses run high. IPC short: longer runs, when the
stream has a run and not a visit share, since a run of one on every
access would undo the dilution the share is; more
chains, then a sequential walk that lets the prefetcher stream, when
the stream has a run to walk and not a visit share, which has none
(beside a neighbour on its sibling threads schbench's replay walked
its share from a fiftieth to all in forty passes with IPC where it
was); under
the L3 miss proxy, which does not count prefetched lines, only with
misses to spare, none asked for or within the absolute floor, since
the walk then costs counted misses. Past
4096 ops per access, with misses not short, the stream is pure
dilution and run length halves each pass, down to one visit per 1024
accesses, since every miss dropped saves a thousand instructions of
compute; the sparse and code
visits may then grow to a quarter of the instructions per access.
With compute at its ceiling, the main stream diluted to its floor
and misses still high, the sparse set halves per pass, down to 128
pages under the L2-hit definition and 4096 under walks: its lines,
one per page, and their page-table entries are refetched after every
idle period of the duty cycle, and by then they are the misses left;
the visit rate makes up for the lower miss rate of the smaller set,
which is why the set does not shrink while the main stream still
makes the misses. A window that moves no knob, every lever its
targets need being at its range, is not a pass, so an unreachable
target does not spend the budget a later disturbance needs. dTLB
events beyond what the sparse stream can account for, at one per
visit, come from the main stream's own pages; when they alone exceed
the target the working set shrinks, by at most half per pass; failing
that, with misses to spare under the proxy or counted misses
otherwise, the sequential share rises when IPC is not high, or visits
per access fall and dilute the events when it is; otherwise dTLB
waits.
A shrink that cost a quarter of the misses beyond what the run length
explains, with misses above their absolute floor and MPKI left short
of what shedding every compute instruction would recover, by the
measured slope, is undone and not tried again, and the window it spoiled moves nothing else, since the
other rules would chase the misses it took away. A target within
tolerance moves none of its knobs, and a window with every judged
target within moves nothing: every rule nudges, a nudge from inside
the band chases noise, and each one is a pass that resets the
averaging of windows; the visit rates still follow a change in
instructions per access, which the model predicts. At one op per
access the IPC and MPKI rules pull the run length and the sequential
share against each other, a prefetched walk or a longer run bringing
the misses the one wants and the IPC the other does not; a lever that
has turned twice within three passes is frozen, since the two targets
meet nowhere along it. Compute is a count, and near one op the MPKI
band can lie between two counts: stress-ng under resctrl swung
between five and six ops for its whole budget, 15.3 and 13.2 misses
per thousand around 14.2 (Zen 4, 2026-10-19), so a move of one op
that turns twice within three passes is frozen the same way, larger
moves being the model settling. Frozen, the fit goes back to the closest pass it saw,
by the sum of its relative errors over tolerance, each capped at ten
tolerances so a target out of reach does not pick the pass by its
noise, and continues from
there with the lever held. Ops
follow the misses per access the run length the IPC rule chose
implies, by the share of the misses that followed the run length last
pass, between none and all, since misses the main stream does not
make, the sparse set's refills after an idle period, do not follow
it; they
move at most fourfold per pass, since the miss proxy counts fewer
misses once compute gives the prefetcher time and a large step lands
far from where the model said; ops stop at 80k and visits at 10k per
access, some 200k instructions, so an access never outlasts the busy
part of a period.

The fit has a budget of under four seconds. Ratios are read over the
whole fit window and the workers take new knobs within a 10 ms
period; measured at fixed knobs on Zen 4, IPC and MPKI vary 0.3 to
0.5 percent from one 100 ms window to the next and the TLB rates about
one percent, far inside the tolerance, so a 100 ms window costs nothing
measurable and thirty-six passes fit the budget; `--window` changes
it. The page-cache file is written by a thread while the first windows
run, so a gigabyte of it does not hold the workers back, and the
seconds it is still being written do not count toward busy cores,
since its CPU is the writer's and no knob intended it: a gigabyte
took two seconds and read as a core where a bursty profile intended a
few hundredths. Consecutive
windows at the same knobs are pooled for the fitter, the newest two
at least and then as many as hold the instructions of 0.4 s at the
run's mean rate, plus two seconds per unit of the burstiness target:
the rates per instruction weighted by each window's instructions and
IPC as the instructions over the cycles, as a recorder's interval
would count them, so a single noisy window, a neighbour's burst, a
page-cache refill or a quiet second of a bursty duty cycle, whose few
instructions carry the misses of a cold cache and the kernel's
wakeups, moves nothing on its own. Those wakeups are a cost per
second, not per instruction: on fio's replay at fixed knobs (Zen 4,
2026-10-18) a four-second plateau at full duty read 0.026 misses and
0.05 iTLB events per thousand instructions and one at a tenth of the
duty 0.17 and 0.09, IPC 0.87 against 0.84 and the dTLB rate 8.46
against 8.25, a refetch per wakeup over instructions that scale with
the second's factor, so a rate measured at factor f is r0 + c/f and
the long-run value a recorder's intervals average, instructions
weighting each second, is the same at the mean of the clipped
factors. The fitter learns c per rate, cycles, misses, dTLB and iTLB
events per instruction, from two pooled windows at the same knobs
whose factors' reciprocals differ by one or more, averaging later
pairs in, and judges every window at the long-run factor, c times the
difference of the reciprocals taken off; until c is known a window is
judged as read, so the first fit lands inside the plateau it sees and
the first plateau change teaches the correction. Busy cores and
burstiness are judged on a rolling window of forty one-second
samples, ten factors, from the third and the twelfth sample on, as
above; until then they are not judged at all, so an early reading of them neither
picks the pass to hold nor keeps a fit whose ratios are at target from
converging. Convergence is declared after two consecutive windows
with every judged target within tolerance; the generator then holds and keeps reporting, and
moves again only after a second of consecutive windows out of
tolerance, at least two, so a noisy window or a few, a neighbour's
burst, a page migration or the moment a recorder attaches, does not
undo the fit; the report counts the fit converged while it holds so,
through windows out for less than that second, since a target at the
edge of its band flickers in and out. Once moving again it fits as at
first, on every window out, until it converges again, since a target
left at the edge of its band by a move would otherwise neither move
nor converge; with `--adaptive` it keeps adjusting. A fit that
runs out of passes without ever converging holds the closest pass it
saw, by that capped sum of relative errors over tolerance, rather than
the last, which is as likely to be a swing as a settling, and says
so; one that has converged is not limited, since it moves only after
a second out. A re-fit, after a second out or on a renewed budget,
judges its closest pass afresh in the new surroundings, and when six
passes bring it no closer it holds that pass: beside a memory-bound
neighbour stress-ng's live fit made sixty-one moves over eight
seconds before holding the pass that was right all along (Zen 4,
2026-10-18); what is out is out of reach at those knobs too, and the
hold moves again when a target it met goes out for a second, as any
hold does, or when a target reads more than twice its distance at
the hold and a tolerance more for a second, per target since a floor
out of reach stands at the cap in every window and would hide the
rest in a sum: a re-fit that held while a neighbour was still
arriving had its closest pass measured before the neighbour's
traffic settled, TaoBench's at an IPC of 0.63 that read 0.17 for the
neighbour's whole stay, and stress-ng's hold beside it, its IPC 0.19,
read 0.55 once the neighbour had left (Zen 4, 2026-10-19). A fit that ends out of tolerance while still moving
reports the closest pass's knobs, which is what a hold of them should
run; one that has stood still for a second has settled and reports
where it stands. The first guess takes
compute from the MPKI target where that asks for far more than the
default, since at eight lines a visit the instructions the misses need
are known to a factor, and growing it from the default a pass at a
time cost the cache-resident profiles most of their passes.

Targets without a verified or proxy measurement take their knobs from
the calibration file for this CPU model, or the built-in defaults, and
are reported as unverified. A fit that converged is kept under
`$XDG_CACHE_HOME/thrashgen`, or `~/.cache/thrashgen`, in a file named
by a hash of the CPU model and the target set, and the next run of the
same targets on the same CPU model starts from it, so a profile
replayed in many containers fits once and the rest start at its
knobs; `--calibration` and `--knobs` take precedence, and `--no-cache`
neither reads nor writes it. DRAM bandwidth without resctrl is modelled
from the counted misses and the bytes the generator wrote
non-temporally or dirtied, reported at the modelled tier and not
fitted: the model knows nothing the misses do not, and fitting a
recording against it set non-temporal lines by formula and took IPC,
MPKI and the TLB rates with them.

## 7. Commands and artifacts

| command | input | output |
| --- | --- | --- |
| `caps` | none | the tier table for this process |
| `record` | `--cgroup PATH`, `--pid N`, `--host`, or `-- cmd args`; interval, duration | `samples.jsonl`: a header line with host and CPU identity, then one line per sample |
| `aggregate` | one or more `samples.jsonl` | `profile.json`: `count`, `cpu`, `avg`, `pctiles`, `provenance` |
| `run` | `profile.json` with `--pick avg\|pNN`, `--set key=value`, or one option per field such as `--ipc`; `--calibration` to start from; `--knobs` to fix, with `--no-counters` to leave the PMU to a recorder; `--no-cache` to neither start from nor keep the cached fit; scratch dir; duration | load, plus `report.json` and a table on stdout |
| `calibrate` | the `run` inputs, on a host with counters; stops once converged | `calibration.json`: knobs, CPU model, threads and tiers |
| `compare` | a profile and a report, recording or profile | per-field error table, optionally JSON |

## 8. Testing

See `docs/testing.md`. In short: pure math and parsers under unit and
property tests with fixtures from real systems; the fitter against a
simulated machine; hardware behind the `hw-tests` feature, failing
rather than skipping when a privilege is missing; and end-to-end
experiments that record a known benchmark, replay it, and plot both.

## 9. Experiments

`docs/experiments.md` describes each one and its plots. The planned
set records schbench, stress-ng, sysbench and fio under known settings,
replays each profile, and compares the recorder's view of the original
with its view of the replay, observable by observable.
