# Status

Where the project stands and what is left, for whoever picks it up.
Dates are absolute. Update this file when an item below closes.

## Verified, as of 2026-10-19

All on one AMD Zen 4 workstation (Threadripper PRO 7975WX, 64 CPUs,
Linux 7.2, THP `always`, `perf_event_paranoid` 2) unless noted.

| claim | how | result |
| --- | --- | --- |
| unit, fixture, CLI and plot tests | `cargo test` | 195 pass |
| lints | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, with and without the features | clean |
| coverage | `cargo llvm-cov --summary-only` | 96% lines, 95% regions |
| counters on the tool's own threads and a fit of counter targets | `cargo test --release --features hw-tests`, unprivileged | 188 unit and 13 CLI tests pass |
| per-CPU, uncore, resctrl, L3 filters | root tests as in `docs/testing.md`, resctrl mounted | pass |
| the generator's per-access costs with the host geometry | fixed-knob runs of the build before the geometry (a15e00f) and this one, interleaved, one op and one lever at a time (`experiments/levers.sh` does the same for one build), and the user-mode replays at their published knobs, 2026-10-18 | instructions per line of a run 13.3 up to 8 lines and 12.7 up to 64, in both builds; 1.6 and 0.93 to 0.95 fills per line at one op; a sparse visit 15.9 instructions against 16.9 at 1.00 dTLB events; a stub call 36 against 38 at 1.03 iTLB events; a non-temporal line 41.0 against 40.8; a page-cache read 117 instructions in user mode and 1791 with 60.9 misses in kernel mode; the replays within 0.5 percent of the old build's instructions per access. With compute between visits the fills per visit differ between builds, 4 to 17 percent fewer and 5 percent more at the same knobs with the hot loops the same instruction for instruction, and moving the batch loop into a function in the old tree alone moved them 4 percent: prefetcher timing, which the fitter's measured elasticity absorbs |
| the fit lands in seconds | the chain's first fits on 2026-10-19 at `33ffb22` | schbench 0.4 s, sysbench 0.6 s, taobench 0.7 s, fio 0.8 s to convergence in user mode; stress-ng meets everything but dTLB (open item 2) by 1.0 s; fio with the kernel in the count by 3.7 s |
| neighbours: three profiles at once, twelve instances, a neighbour coming and going | `MODE=noisy`, `MODE=crowd` at `33ffb22`, `MODE=disturbed` at `d393c34`, 2026-10-19 | `docs/experiments.md`: with two neighbours TaoBench lands every field and stress-ng everything but its floor, schbench's IPC 6 percent over; a full host takes schbench's IPC to 2.93 from 3.74, a sibling sharing each core, TaoBench's four copies land every field and stress-ng's hold a compromise on a saturated bus; a fit follows a neighbour's arrival and departure within about a second and is back within on every reachable field once it has left, its re-fit bounded by the hold for lack of progress and the renewal of a hold whose targets go much further out |
| end-to-end replays, five modes | `experiments/all.sh` with `MODE=user`, `kernel`, `resctrl`, `perfmon`, 2026-10-19 at `33ffb22` on a quiet host | `docs/experiments.md`: schbench, sysbench and TaoBench within tolerance on every field in every mode, converged in 0.4 to 0.7 s, TaoBench's re-fits under resctrl and the L3 PMU holding their closest pass after six passes without progress, everything within; fio 10 of 10 in user and kernel mode, 9 of 10 under resctrl (MPKI) and under the L3 PMU (MPKI a third over); stress-ng 9 of 10 but for its dTLB floor, 9 of 11 under resctrl where its 70 GB/s is beyond its counted misses, 8 under the L3 PMU; measured bandwidth within for every profile that can reach it, 4 percent under for TaoBench |

## Verified on an Apple M2 Ultra, as of 2026-10-19

Asahi Linux 7.1 with 16 KiB pages (`aarch64+16k`), 24 cores in two
classes, 16 Avalanche P cores and 8 Blizzard E cores, each class with
a PMU of its own (`apple_avalanche_pmu`, `apple_blizzard_pmu`), THP
`madvise` with 32 MiB hugepages, `perf_event_paranoid` 2, no resctrl
and no L3 in sysfs: the L2 is 16 MiB per P cluster and 4 MiB per E
cluster, the system cache behind them is not listed, and sysfs gives
no cache line size where `CTR_EL0` says 64 bytes and the L2 moves 128.

What differed from the x86 assumptions, and what was done about it:

| assumption | this host | change |
| --- | --- | --- |
| one core PMU at `PERF_TYPE_RAW` | two, one per class of core; a task event opened without naming a PMU lands on whichever accepts it first and counts only there | the plan opens the core set on every class's PMU under one name, hardware events with the PMU type in the config's high half, and `perf::fold_by_name` sums a name's readings and scales the sum by the time on no PMU at all |
| an L3 miss event in the kernel's tables | none listed; a sweep of every event number found `0x23` on the P cores counting one per 128-byte line a demand access fetched from memory and nothing prefetched; the E cores have no such event | the `applem2` table, with the event per class; the plan's note says the E cores add instructions but no misses |
| TLB walks for the TLB rates | `L2_TLB_MISS_DATA` counts one per missing access where the walk event counts 1.5 on the E cores; the instruction-side L2 miss and walk events count speculative fetches, in bursts of hundreds per stub call at a low call rate | dTLB from the L2 miss, iTLB from `L1I_TLB_MISS_DEMAND`, steady at 1.2 per call |
| 4 KiB pages, 64-byte lines, 64 lines a run, a page-cache read of 4 KiB | 16 KiB pages, 128-byte lines | the streams and the fitter take a `Geometry` from the host and the table |
| a run's lines each miss | a visit's second and further lines in a page, consecutive, strided or scattered, are fetched by the region prefetcher and not counted: 1.0 counted per visit of 2 to 128 lines | on a demand-only host each line of a run lands in a page of its own, independent: 2 lines count 2.0, 8 count 8.0 with compute between visits |
| CPUs alike, numbered from the fastest | the E cores are numbered first | pinning takes the highest `cpu_capacity` first |
| `/proc/cpuinfo` names the CPU | no model name on Arm | the device tree's `model` names the machine and its chip |

| claim | how | result |
| --- | --- | --- |
| unit, fixture, CLI and plot tests | `cargo test`, 2026-10-19 at `57081ab` | 197 pass |
| lints | `cargo fmt --check`, clippy with and without the features | clean |
| counters on the tool's own threads and a fit of counter targets | `cargo test --release --features hw-tests`, unprivileged | 189 unit and 13 CLI tests pass, the fit of IPC 0.8, 8 MPKI, 1 dTLB and 0.05 iTLB events per thousand on real hardware among them |
| the generator's per-unit costs with the access path compiled per geometry | `experiments/levers.sh` on this build and on `7e9fcdb`, the build before, twice each, back to back, 2026-10-19, and fixed-knob runs of both | events per unit the same within the run-to-run spread: 1.003 L2 TLB misses a sparse visit, 8.0 counted misses for eight lines in pages of their own with 64 ops between visits in both builds, 1.0 counted miss an access with eight non-temporal lines, none from the lines themselves; a stub call 1.0 to 1.4 demand L1 iTLB misses from one run to the next on either build, at one call in two hundred accesses and at one in twenty alike, where within a run the rate holds to a percent. Instructions per unit fell: a line of a run 26 (1 to 8) and 23 (8 to 64) against 43 and 42, a sparse visit 11.9 against 14.9, a stub call 24 against 41, a non-temporal line 31 against 76, a page-cache read 115 in user mode in both, an op 3.45 against 3.37. The access itself at one op and no lever costs 90 instructions on this build against 60 on the old, and with 64 ops and a stub call on one access in twenty 250, 261 and 280 on the old build, a one-line variant of this one and this one: codegen on this target moves the fixed cost per access by tens of instructions from build to build, which the fitter's measured instructions per access absorb and the per-unit figures above do not include |
| the code stubs with their instruction-cache maintenance, the `stnp` stores, the sparse walk on 16 KiB pages | the code-pages test, now on both architectures, and fixed-knob runs: 1.2 iTLB events and 1.0 L2 TLB misses per visit, 1.0 counted miss per non-temporal line's access at IPC 1.4 | pass |
| a fit of IPC 0.8, 8 MPKI, 1 dTLB and 0.05 iTLB events per thousand on two P cores | `run --set ...` for 8 s | every target within by the fifth pass, 1 s |
| root tests | the lib test binary run as root | per-CPU counters open; the AMD L3 filter test and the resctrl group test fail by design, this host having neither |
| end-to-end replays | `experiments/all.sh` with `MODE=user`, 2026-10-19 at `57081ab` | `docs/experiments.md`: stress-ng within tolerance on every field; schbench, sysbench and TaoBench on every ratio, converged in 0.3 to 1.5 s, the memory total of the two small processes a floor over and TaoBench's iTLB rate 5 percent over; fio 9 of 10, its iTLB rate 47 percent over (open item 7) |
| end-to-end replays, the other modes | `MODE=kernel`, `perfmon`, `noisy`, `crowd`, `disturbed`, 2026-10-19 at `57081ab`; `resctrl` cannot run here | `docs/experiments.md`: kernel 10, 9, 10, 10 and 8 of 10 for schbench, sysbench, TaoBench, stress-ng and fio; perfmon 10, 9, 10, 10, 5, fio's kernel-mode profile out of reach without a page-cache lever (open item 3); noisy 10 of 10 for all three; crowd, twelve instances on twenty-four cores, 8, 6 to 7 and 8 of 10 at a fair share of the cores; disturbed within on every field beside the neighbour and after it |

Not verified anywhere:

- the Intel and Arm Ltd. event tables come from the kernel's
  `pmu-events` at v7.1 and are checked only against fixtures, never
  on that hardware; Apple's M1 cores share the driver's event list
  with the M2's but not the sweep, so they get no table;
- the replay of a container across hosts: every replay here ran on
  the host that recorded it.

## Open items

In rough order of value. The first four and the last two are from the
Zen 4 host; the M2 Ultra's are between.

1. **Bandwidth measurement and levers.** Under resctrl the bandwidth
   rule has three levers, the write share, non-temporal lines and
   page-cache reads where the first two cannot carry it; the first two
   move IPC as well, so the rule waits for IPC and MPKI to be near
   their targets, and a model of what each costs in IPC would let it
   move earlier. Page-cache reads are a lever only over a file larger
   than the L3, since a resident file is copied from cache: in the
   chain of 2026-10-18 stress-ng's one-page file was read twelve times
   per access, moved no bytes and diluted MPKI by a fifth, and the
   run-length rule chased the loss to its cap. The reads now need the
   larger file, and in user mode, where a read is 115 instructions
   without a miss, the rule adds only as many as leave MPKI in its
   band. Bandwidth beyond what the counted misses move, stress-ng's
   70 GB/s at 21 MPKI, is physics: prefetch and streaming stores the
   miss proxy does not count, where the generator's bytes follow its
   misses, so the rule waits there. The rule also rests while
   bandwidth is within its absolute floor, which it checked by the
   relative band alone until 2026-10-19, when a 6.5 MB/s target read
   at 24 MB/s halved sysbench's write share on every pass another
   target made. In the chain of 2026-10-19 measured bandwidth landed
   within for every profile that can reach it, TaoBench's 4 percent
   under. On this host resctrl's monitoring runs in its
   `default` assignment mode, the only one it offers, where the
   kernel's documentation warns that the hardware reallocates counters
   and readings come back misleading or `Unavailable`; a probe showed
   a single thread at half duty losing its counter within seconds,
   pinned or not, while the busy multi-thread replays read fine and
   TaoBench's bandwidth landed within 2 percent. fio's replay has no
   bandwidth reading for that reason. A host with the `mbm_event` mode
   would not have the problem; the tool could select it when it mounts
   resctrl itself. One more host fact: resctrl mounts once per kernel,
   so a container started while it is mounted keeps it in its own
   mount namespace after the host unmounts it, and the host cannot
   mount it again (`mount` says busy at a path that is not a mount
   point) until that namespace drops it. On 2026-10-18 a podman
   container held it for half a day; `umount -N` from the host did
   not reach the propagated mount, restarting the container did.
2. **stress-ng's dTLB rate is a hardware floor here.** Under the
   L2-hit definition this Zen 4 reloads its L1 dTLB on about 1.4
   percent of hugepage touches whatever the set, measured for random
   single-line visits over 13 to 56 hugepages, and its capacity edge
   lies between 56 and 112. A memory-bound target at low IPC needs
   dependent misses, each in a page of its own, so it keeps a few
   hundredths of a dTLB event per thousand instructions where
   stress-ng's sequential stores show a few thousandths; a sequential
   walk meets the dTLB rate but lifts IPC to 0.74 against 0.22. The
   fitter waits, as designed; a per-CPU-model table of the floor would
   let it say so up front. See `docs/design.md` section 5.1.
3. **Profiles whose misses are the kernel's.** The page-cache read
   knob gives the generator the kernel's copies, and with the kernel
   in the count fio's kernel-mode profile is reached, IPC, MPKI and
   dTLB within tolerance for seconds at a time, but not held: at 28
   percent duty with a CV of one the quiet seconds carry the misses of
   cold caches and the kernel's wakeups, the pooled windows span them
   only in part, and the fit moves again. The same shows in user mode:
   fio's rates per instruction differ several times over between a
   quiet four-second plateau, where every wakeup refetches a cold cache
   and TLB, and a busy one. In the chain of 2026-10-18 the fit
   converged in 1.4 s inside one plateau and the hold's iTLB rate
   averaged three times the target over all of them; in an earlier
   run the fit moved again at the first busy plateau, where MPKI and
   the iTLB rate were five to twenty times lower, and settled by 4.4 s
   with every field of the hold within. Measured on the replay at
   fixed knobs with one-second grouping (2026-10-18): the rates follow
   a constant cost per wakeup over the second's instructions, MPKI
   0.026 at full duty and 0.17 at a tenth of it, the iTLB rate 0.05
   and 0.09, IPC 0.87 and 0.84, so a rate at factor f is r0 + c/f and
   the long-run value is the same at the mean factor. The fitter now
   learns c per rate from two plateaus at the same knobs and judges
   every window at the long-run factor (design section 6, with a
   plant and a step test), so the plateau the first fit lands in no
   longer decides what the hold averages; the kernel-mode profile's
   cold-cache misses are the same cost and get the same correction.
   What is left is physics the hold cannot help: a quiet second of
   the replay still carries the refetch, as the original's did. With
   the recording started once fio's reads run from the page cache, the
   chain of 2026-10-19 landed fio within on every field in user and
   kernel mode and on 9 of 10 under resctrl and the L3 PMU, its MPKI
   the field out there. Under `CAP_PERFMON` the L3
   PMU counts prefetched lines too, so stress-ng reads 95 MPKI; a miss
   every ten instructions needs the prefetched walk, which lifts IPC,
   and the two targets meet nowhere, so the fit now freezes the lever
   they fight over and holds the closest pass as the compromise, by
   2.9 s, where it used to cycle through its whole budget.

4. **What a neighbour takes.** Beside a memory-bound neighbour a
   replay cannot have the IPC its profile recorded alone: schbench's
   on a core shared with a sibling thread, stress-ng's on a saturated
   bus. The fit tries anyway: stress-ng's live fit spent two budgets
   of passes, sixty-one moves over eight seconds, before holding its
   closest pass, which was the right one once the neighbour left. A
   re-fit now judges its closest pass afresh in the new surroundings
   and, five seconds in, holds it after six passes without a closer
   one, and a hold renews when a target it met goes out or one goes
   much further out (design section 6): on 2026-10-19 stress-ng spent
   one budget and one renewal on the arrival, 49 passes over 11 s,
   and its hold renewed within a second of the departure, where it
   used to spend two budgets and keep its neighbour-time knobs;
   TaoBench's re-fit reached an IPC of 0.50 against 0.64 beside the
   neighbour and every field after it. The comparison itself is
   against a profile recorded alone, which is the question asked, and
   a profile recorded beside the same neighbour would be the other
   question.

5. **Misses on the E cores.** Apple's E-core PMU has no event that
   counts fetches from memory, so a profile's misses per instruction
   on an M2 are those of its P-core time over the instructions of
   both, and a replay that lands the same ratio has made the same
   compromise. A recorder could carry the share of instructions
   retired on cores without the event, so a reader knows how much of
   the denominator the numerator does not cover.
6. **A zero target.** stress-ng's sequential writes record exactly no
   counted misses on this host, and the ops rule, which divides by the
   target, never moved: the first user-mode chain left its replay at
   15 MPKI and an IPC of 0.86 against 2.0. The rule now aims at half
   the absolute floor and stress-ng lands every field in every mode;
   the same guard may be wanted wherever a rule divides by a target
   that a host can record as zero.
7. **Stub calls on this host.** The instruction-side L2 TLB miss and
   walk events count speculative fetches, and at a call on one access
   in two hundred they came in bursts of hundreds per call for
   seconds at a time; the demand L1 miss event holds within a run and
   is what the table uses, but reads 1.0 to 1.4 per call from one run
   to the next at the same knobs (2026-10-19), so the rate a fit lands
   is not the rate its hold reads: TaoBench's and fio's iTLB rates in
   user mode, 5 and 47 percent over. The stream sets' floor of 2 MiB
   each is 128 pages here against 512 on 4 KiB pages, and 128 stub
   pages are within the L1 iTLB's reach, so a call brings a hundredth
   of the events it would past it and fio's replay makes 39 calls an
   access; a floor in pages rather than bytes would give the lever
   back. Whether the bursts are the front end fetching a stale
   predicted target is not known; a call to a hot page on every
   access did not quiet them.
8. **The page-cache lever needs an L3 size.** Page-cache reads are a
   lever only over a file larger than the L3, and sysfs on this host
   lists no L3, so the reads are never a lever and a profile whose
   misses are the kernel's copies, fio's with the kernel counted, is
   out of reach: 8 of 10 in kernel mode and 5 under `CAP_PERFMON`. The
   system cache is 48 MiB a die; a table of it per chip, or a probe
   of the miss rate against the file size, would let the lever in.

9. **The L3 PMU counts the whole domain.** Under `CAP_PERFMON` the
   recorder's L3 misses come from the L3 PMU filtered to the scope's
   cores, and a cgroup that may run anywhere has every core, so any
   neighbour's misses land in the recording and the replay alike: an
   indexer from another session beside the chain of 2026-10-19 had
   sysbench read 5.7 misses per thousand instructions where it reads
   0.05 alone. The mode needs a quiet host or a pinned cgroup; the
   recorder could say which it had.
10. **Kernel memory that does not come back.** The empty files that
   stand in for kernel memory cost more slab than estimated on btrfs,
   and when the closed loop removes them the slab is not reclaimed
   during a quiet hold: sysbench's replay under resctrl ended 9 MiB
   over its 4 MiB of memory on 2026-10-19, within the 8 MiB floor on
   the chain's other runs. Work: a cheaper stand-in for kernel memory,
   or a floor the hold reports as the kernel's.

Closed since 2026-10-16: the fast fit closed the pass-budget item and
fio's TLB rate; busy cores and burstiness are judged from the third
second; a repeat fit starts from its cached knobs; and the dTLB
disagreement between a fitting run and a recorder sharing the PMU did
not reproduce, the two agreeing within one percent on every ratio over
25 s of TaoBench, so the harness's counter-free hold is a precaution
rather than a fix.

## Conventions

`AGENTS.md` has the build, test and commit rules. The design doc is
the contract: a change in what an observable means or how a knob
moves it lands in `docs/design.md` in the same commit as the code.
Experiment outputs that are kept go under `docs/experiments/<mode>/`;
everything else from a run stays in the gitignored
`experiments/out/`.
