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
| `memory_limit_bytes` | bytes | hint, not enforced | cgroup `memory.max`; absent when `max` | same |
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
charged to the generator's cgroup, and topped up as the kernel
reclaims it.

Each field in a profile carries provenance: the scope (`cgroup`,
`process`, `host`), the counting mode (`user`, `all`) and the method
(`perf`, `cgroup`, `proc`, `resctrl`, `proxy`, `model`).

### 2.1 Vendor events

Vendors count different things, so a profile is comparable only on its
own vendor's definitions. The tables live in `src/events/` with fixtures
copied from the kernel's `tools/perf/pmu-events` JSON at v7.1.

| field | AMD Zen 3, 4, 5 | Intel Skylake-SP and later server cores | ARM Neoverse |
| --- | --- | --- | --- |
| L3 misses | L3 PMU lookup-miss event, raw `amd_l3/event=0x04,umask=0x01/` on Zen 4 and 5 and `event=0x06,umask=0x01` on Zen 3, per L3 domain, filtered to the scope's cores with the `coreid` / `threadmask` fields. Unprivileged proxy: core event `ls_any_fills_from_sys.dram_io_all`, raw `event=0x44,umask=0x48`, data cache fills from DRAM or MMIO | `LONGEST_LAT_CACHE.MISS`, `event=0x2e,umask=0x41` | `LL_CACHE_MISS_RD`, `0x37`, reads only |
| dTLB | L1 dTLB misses that hit the L2 TLB: raw `event=0x45,umask=0x0f` | `DTLB_LOAD_MISSES.WALK_COMPLETED` plus `DTLB_STORE_MISSES.WALK_COMPLETED`; `0x08` and `0x49` with umask `0x0e` through Ice Lake-SP and on Sierra Forest, `0x12` and `0x13` from Sapphire Rapids | `DTLB_WALK`, `0x34` |
| iTLB | L2 iTLB misses: raw `event=0x85,umask=0x0f` | `ITLB_MISSES.WALK_COMPLETED`; `0x85` or `0x11`, umask `0x0e`, split as above | `ITLB_WALK`, `0x35` |

CPU models are mapped to tables the way the kernel's `mapfile.csv`
does it. A model with no verified table gets no counters rather than a
guess, and the report says so.

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

## 5. Load generator

Each worker thread repeats one *access*: a few memory streams, then
some compute, inside a duty-cycled 10 ms period. Addresses come from a
PRNG, not from loaded data, so misses overlap.

| knob | moves | mechanism |
| --- | --- | --- |
| `threads` | busy cores | default `cpu_limit_cores`, capped at the CPUs in the affinity mask; each worker pinned to one CPU so uncore attribution is possible |
| `duty` | busy cores | busy fraction of each 10 ms period |
| `cv` | burstiness | per-second factor from a lognormal with mean 1 and CV `cv` (σ² = ln(1 + cv²), μ = −σ²/2); seeded from the wall-clock second so all threads move together |
| `ops`, `chains` | instructions per access, so IPC and every per-instruction rate | `chains` independent ALU dependency chains, `ops` iterations, kept alive with an empty `asm!` barrier |
| `lines` | L3 misses per access | consecutive lines touched in a random page of the main buffer; longer runs let the prefetcher hide latency, raising IPC at equal MPKI |
| `wfrac` | write share | fraction of main-stream lines stored to |
| `ntlines` | DRAM bandwidth beyond L3 misses | full lines written with non-temporal stores |
| `tlbvisits`, sparse page count | dTLB rate | one fixed line per page over a page set sized between the L1 and L2 dTLB reach (AMD definition) or beyond the L2 reach (walks) |
| `codevisits`, code page count | iTLB rate | calls through a function pointer into one-instruction stubs placed one per executable page at varying offsets; `codevisits` may be below one per access |
| `huge` | main-stream page walks | `MADV_HUGEPAGE` on the main buffer; the report shows what THP actually backed |

Footprint: the main buffer is anonymous memory touched once at start;
page cache is a file of `pagecache_bytes` in a disk-backed scratch
directory, written then read, with residency re-checked and restored;
shmem is a `memfd`; kernel memory is empty files as in section 2.

## 6. Fitting

Knobs depend on the hardware, so they are fit per CPU model and
cached. Order: footprint first and let it settle; threads and duty for
busy cores; chains, then ops and lines for IPC and MPKI; ntlines for
bandwidth; tlbvisits; codevisits; repeat until every verified target is
within tolerance or the pass limit is hit.

Each knob moves by the factor target / achieved, damped by half on each
pass. Ratios settle within a 30 s window. Busy cores and burstiness do
not: with independent per-second jitter a window's mean has standard
error `cv / sqrt(seconds)`, so tolerance t needs `(cv / t)²` seconds;
the fitter sizes those windows from the target CV, with 300 s as the
ceiling. Convergence is declared after two consecutive windows within
tolerance. Then the generator holds and keeps reporting; `--adaptive`
keeps adjusting instead.

Targets without a verified or proxy measurement take their knobs from
the calibration file for this CPU model, or the built-in defaults, and
are reported as unverified.

## 7. Commands and artifacts

| command | input | output |
| --- | --- | --- |
| `caps` | none | the tier table for this process |
| `record` | `--cgroup PATH`, `--pid N`, `--host`, or `-- cmd args`; interval, duration | `samples.jsonl`: a header line with host and CPU identity, then one line per sample |
| `aggregate` | one or more `samples.jsonl` | `profile.json`: `count`, `cpu`, `avg`, `pctiles`, `provenance` |
| `run` | `profile.json` with `--pick avg|pNN`, or `--set key=value`; scratch dir; duration | load, plus `report.json` and a table on stdout |
| `calibrate` | profile, on a host with counters | `calibration.json` keyed by CPU model |
| `compare` | a profile and a report or recording | per-field error table |

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
