# Status

Where the project stands and what is left, for whoever picks it up.
Dates are absolute. Update this file when an item below closes.

## Verified, as of 2026-10-18

All on one AMD Zen 4 workstation (Threadripper PRO 7975WX, 64 CPUs,
Linux 7.2, THP `always`, `perf_event_paranoid` 2) unless noted.

| claim | how | result |
| --- | --- | --- |
| unit, fixture, CLI and plot tests | `cargo test` | 183 pass |
| lints | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, with and without the features | clean |
| coverage | `cargo llvm-cov --summary-only` | 96% lines, 95% regions |
| counters on the tool's own threads and a fit of counter targets | `cargo test --release --features hw-tests`, unprivileged | pass |
| per-CPU, uncore, resctrl, L3 filters | root tests as in `docs/testing.md`, resctrl mounted | pass |
| the fit lands in seconds | cold fits of the recorded profiles on this host, `thrashgen run orig.json --duration 8 --no-cache` | schbench 0.3 s, sysbench 0.6 s, taobench 0.6 s to the last knob change; stress-ng meets everything but dTLB (open item 2) by 1.0 s; fio converges by 1.4 s and re-fits when its duty cycle changes plateau (open item 3) |
| neighbours: three profiles at once, twelve instances, a neighbour coming and going | `MODE=noisy`, `MODE=crowd`, `MODE=disturbed` | `docs/experiments.md`: with neighbours every profile keeps every field it can reach; a full host takes schbench's IPC to 2.85 from 3.71, a sibling sharing each core, and ten to twenty percent of stress-ng's ratios on a saturated bus; a fit follows a neighbour's arrival and departure within about a second and is back within on every reachable field once it has left |
| end-to-end replays, five modes | `experiments/all.sh` with `MODE=user`, `kernel`, `resctrl`, `noisy`, `perfmon` | `docs/experiments.md`: schbench, sysbench and TaoBench within tolerance on every field in every mode, converged in 0.3 to 0.9 s, with neighbours and under the L3 PMU too; stress-ng 9 of 10 but for its dTLB floor, with neighbours too, 8 under the L3 PMU; fio 9 of 10 in user mode, 8 under resctrl, 6 to 7 where the profile carries kernel-mode misses; bandwidth within its floor for the compute-bound profiles and 5 percent over for TaoBench |

Not verified anywhere:

- the CI workflow in `.github/workflows/ci.yml`, lints only by choice,
  has never run; the repository has no remote;
- the Intel and Arm event tables come from the kernel's `pmu-events`
  at v7.1 and are checked only against fixtures, never on that
  hardware;
- the AArch64 generator paths, the stubs with their instruction-cache
  maintenance and the `stnp` non-temporal stores, compile for that
  target and have never run on it; the streams assume 4 KiB pages and
  64-byte lines, and the sparse set is sized for a 4 KiB page;
- the replay of a container across hosts: every replay here ran on
  the host that recorded it.

## Open items

In rough order of value.

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
   misses, so the rule waits there. On this host resctrl's monitoring runs in its
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
   with every field of the hold within. A fit that judged a bursty
   profile over whole plateaus, or corrected the rates by the duty of
   the second, would not depend on which plateau it saw. Under `CAP_PERFMON` the L3
   PMU counts prefetched lines too, so stress-ng reads 95 MPKI; a miss
   every ten instructions needs the prefetched walk, which lifts IPC,
   and the two targets meet nowhere, so the fit now freezes the lever
   they fight over and holds the closest pass as the compromise, by
   1.6 s, where it used to cycle through its whole budget.

4. **What a neighbour takes.** Beside a memory-bound neighbour a
   replay cannot have the IPC its profile recorded alone: schbench's
   on a core shared with a sibling thread, stress-ng's on a saturated
   bus. The fit tries anyway: stress-ng's live fit spent two budgets
   of passes, sixty-one moves over eight seconds, before holding its
   closest pass, which was the right one once the neighbour left. A
   fit that knew which targets a neighbour makes unreachable, from
   the IPC's response to its own levers, could hold sooner; the
   comparison itself is against a profile recorded alone, which is
   the question asked, and a profile recorded beside the same
   neighbour would be the other question.

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
