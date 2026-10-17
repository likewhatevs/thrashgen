# Status

Where the project stands and what is left, for whoever picks it up.
Dates are absolute. Update this file when an item below closes.

## Verified, as of 2026-10-16

All on one AMD Zen 4 workstation (Threadripper PRO 7975WX, 64 CPUs,
Linux 7.2, THP `always`, `perf_event_paranoid` 2) unless noted.

| claim | how | result |
| --- | --- | --- |
| unit, fixture, CLI and plot tests | `cargo test` | 159 pass |
| lints | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, with and without the features | clean |
| coverage | `cargo llvm-cov --summary-only` | 96% lines, 94% regions |
| counters on the tool's own threads and a fit of counter targets | `cargo test --release --features hw-tests`, unprivileged | pass |
| per-CPU, uncore, resctrl, L3 filters | root tests as in `docs/testing.md`, resctrl mounted | pass |
| the fit lands in thirty seconds | every converged replay in `docs/experiments.md` converged between 8 and 28 s | yes |
| end-to-end replays, four modes | `experiments/all.sh` with `MODE=user`, `kernel`, `resctrl`, `noisy` | `docs/experiments.md`: compute-bound profiles within tolerance on every field in every mode; TaoBench 8 to 10 of 10; stress-ng 7 to 8; fio 5 to 8 |

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

1. **The hard profiles at fifteen passes.** TaoBench converges in
   some runs and holds a few percent off in others; stress-ng's MPKI
   moves between runs because its run length is still settling when
   the passes run out. More passes, or a calibration from an earlier
   run through `--calibration`, close this; a better first guess for
   memory-bound profiles would too. See `docs/experiments.md`.
2. **Bandwidth for memory-bound profiles.** Measured through resctrl,
   the bandwidth rule's levers, the write share and non-temporal
   lines, move IPC as well, and taobench's IPC drifted 28 percent high
   while they moved. The rule now waits for IPC and MPKI to be near
   their targets; a model of what each lever costs in IPC would let it
   move earlier. stress-ng's 70 GB/s is a streaming store's figure and
   out of reach for random 64-byte lines on this host.
3. **fio's TLB rate.** The sparse page set is capped at an eighth of
   the anonymous footprint so a small process is not swamped by the
   stream's own pages; at 3.4 MB that is a hundred pages, which the
   first-level TLB mostly holds. Visiting the page-cache file's pages
   through a mapping instead would give the stream pages that count
   as page cache, which the profile wants anyway.
4. **stress-ng's dTLB rate** is physics on this host: a random stream
   over hugepages needs a set below the first-level TLB's reach and
   above the L3's size at once, and the two meet where the set sits.
   A per-CPU-model table of the two reaches would let the fitter say
   so up front instead of trying.
5. **Kernel-mode work.** In the `kernel` mode the profiles acquire
   the kernel's misses and TLB events; the generator produces none.
   A knob that makes system calls per access would be the way in.
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
