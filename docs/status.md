# Status

Where the project stands and what is left, for whoever picks it up.
Dates are absolute. Update this file when an item below closes.

## Verified, as of 2026-10-17

All on one AMD Zen 4 workstation (Threadripper PRO 7975WX, 64 CPUs,
Linux 7.2, THP `always`, `perf_event_paranoid` 2) unless noted.

| claim | how | result |
| --- | --- | --- |
| unit, fixture, CLI and plot tests | `cargo test` | 164 pass |
| lints | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, with and without the features | clean |
| coverage | `cargo llvm-cov --summary-only` | 96% lines, 94% regions |
| counters on the tool's own threads and a fit of counter targets | `cargo test --release --features hw-tests`, unprivileged | pass |
| per-CPU, uncore, resctrl, L3 filters | root tests as in `docs/testing.md`, resctrl mounted | pass |
| the fit lands in seconds | cold fits of the recorded profiles on this host, `thrashgen run orig.json --duration 8` | schbench 0.3 s, sysbench 0.6 s, taobench 1.0 s, fio 1.3 s to the last knob change; stress-ng meets everything but dTLB (open item 2) |
| end-to-end replays, five modes | `experiments/all.sh` with `MODE=user`, `kernel`, `resctrl`, `noisy`, `perfmon` | `docs/experiments.md`: compute-bound profiles within tolerance on every field in every mode, fits converged in 0.3 to 1.8 s; TaoBench 10 of 10 in user, kernel and resctrl modes, 9 with neighbours and under the L3 PMU; stress-ng 9 of 10 but for its dTLB floor, 7 with neighbours and under the L3 PMU; fio 9 of 10 in user mode, 5 to 7 where the profile carries kernel-mode misses |

Not verified anywhere:

- the CI workflow in `.github/workflows/ci.yml` has never run; the
  repository has no remote;
- the Intel and Arm event tables come from the kernel's `pmu-events`
  at v7.1 and are checked only against fixtures, never on that
  hardware;
- the replay of a container across hosts: every replay here ran on
  the host that recorded it.

## Open items

In rough order of value.

1. **Bandwidth for memory-bound profiles.** Measured through resctrl,
   the bandwidth rule's levers, the write share and non-temporal
   lines, move IPC as well, and taobench's IPC drifted 28 percent high
   while they moved. The rule now waits for IPC and MPKI to be near
   their targets; a model of what each lever costs in IPC would let it
   move earlier. stress-ng's 70 GB/s is a streaming store's figure and
   out of reach for random 64-byte lines on this host.
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
3. **Kernel-mode work and counted misses.** In the `kernel` mode the
   profiles acquire the kernel's misses and TLB events; the generator
   produces none. fio's 2.4 misses per thousand instructions are
   page-cache copies over a 2 MiB anonymous footprint, which a main
   stream over the page-cache mapping could make and a knob that makes
   system calls per access could make faithfully. Under `CAP_PERFMON`
   the L3 PMU counts prefetched lines too, so stress-ng reads 95 MPKI
   and fio 15.6; a miss every ten instructions needs the prefetched
   walk, which lifts IPC as it lifts MPKI, and the IPC and MPKI rules
   then trade the sequential share and the run length back and forth
   until the passes run out. A joint rule for the two at one op per
   access, or accepting the IPC the walk gives, would settle it.
4. **Busy cores and burstiness take thirty seconds.** They are judged
   only once the rolling window holds thirty one-second samples, since
   the per-second CV has no shorter estimate; the ratios converge in
   about a second and the generator runs the clipped-lognormal model's
   duty and CV until then. A harness fit of ten seconds never judges
   them; the hold recording is where they show.
5. **Warm starts.** `--calibration` carries knobs for targets the host
   cannot measure. A cache keyed by CPU model and profile hash would
   start a repeat fit at the knobs it ended on last time; not started.
6. **The first dTLB measurement.** Run and recorder once disagreed on
   a replay's dTLB rate by fifty times while both shared the PMU; the
   harness now records a counter-free run, which removed it, but the
   mechanism, time-multiplexing against a 10 ms duty cycle, was not
   pinned down.

## Conventions

`AGENTS.md` has the build, test and commit rules. The design doc is
the contract: a change in what an observable means or how a knob
moves it lands in `docs/design.md` in the same commit as the code.
Experiment outputs that are kept go under `docs/experiments/<mode>/`;
everything else from a run stays in the gitignored
`experiments/out/`.
