# Status

Where the project stands and what is left, for whoever picks it up.
Dates are absolute. Update this file when an item below closes.

## Verified, as of 2026-10-16

All on one AMD Zen 4 workstation (Threadripper PRO 7975WX, 64 CPUs,
Linux 7.2, THP `always`, `perf_event_paranoid` 2) unless noted.

| claim | how | result |
| --- | --- | --- |
| unit, fixture, CLI and plot tests | `cargo test` | 141 pass |
| lints | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, with and without the features | clean |
| coverage | `cargo llvm-cov --summary-only` | 96% lines, 94% regions |
| counters on the tool's own threads | `cargo test --features hw-tests`, unprivileged | pass |
| per-CPU, uncore, resctrl, L3 filters | root tests as in `docs/testing.md`, resctrl mounted | pass |
| end-to-end replays | `experiments/all.sh`, unprivileged | schbench and sysbench within tolerance on every field; stress-ng 7 of 10, fio 5 of 10, with the misses explained in `docs/experiments.md` |

Not verified anywhere:

- the CI workflow in `.github/workflows/ci.yml` has never run; the
  repository has no remote;
- the Intel and Arm event tables come from the kernel's `pmu-events`
  at v7.1 and are checked only against fixtures, never on that
  hardware;
- behaviour with `CAP_PERFMON`: the root tests cover the counters, but
  no experiment has recorded or replayed with kernel-mode counting;
- DRAM bandwidth as measured: every experiment ran without resctrl,
  so bandwidth was modelled, and the model has not been checked
  against a resctrl group over a whole replay.

## Open items

In rough order of value.

1. **A sequential or working-set mode for the main stream.** The
   stream draws random pages from the whole main buffer, so a
   sequential, TLB-friendly workload such as stress-ng's store stream
   cannot be matched on IPC, MPKI and dTLB at once, and a
   cache-resident workload is matched by diluting rates with compute
   rather than by shrinking the footprint touched. `docs/design.md`
   section 5.1 describes it; the stream lives in `src/loadgen.rs` and
   the rules that would drive the knob in `src/fit.rs`.
2. **Kernel-mode experiments.** Run the set with `CAP_PERFMON` or
   `perf_event_paranoid` at 1 so both recordings count all modes, and
   see how much of fio's gap closes. `experiments/run.sh` uses
   transient user scopes, so a privileged run needs system scopes.
3. **Bandwidth experiment under resctrl.** Mount resctrl, give the
   recorder and the replay a monitoring group, and compare measured
   bandwidth with the model the unprivileged replay uses.
4. **Named per-field flags.** `run` and `calibrate` take any field
   through `--set key=value`; there is no `--ipc 1.2`. If wanted,
   generate one option per entry of `FIELDS` in `src/profile.rs` so
   the two cannot drift.
5. **`memory_limit_bytes`.** Accepted, aggregated and carried as a
   hint, read by nothing, as the specification's "not enforced"
   allows. A warning when the memory targets exceed it would cost a
   few lines in `src/run.rs`.
6. **TaoBench** in the experiment set once DCPerf is installed; the
   harness takes any command.
7. **License.** None chosen; `Cargo.toml` has no `license` field.

## Conventions

`CLAUDE.md` has the build, test and commit rules. The design doc is
the contract: a change in what an observable means or how a knob
moves it lands in `docs/design.md` in the same commit as the code.
Experiment outputs that are kept go under `docs/experiments/<name>/`;
everything else from a run stays in the gitignored
`experiments/out/`.
