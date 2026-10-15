# Testing

Three layers, each with a different claim.

## Unit and fixture tests: `cargo test`

Run everywhere, including CI, with no privileges and no hardware
assumptions. They cover:

- the pure math: burstiness, ratio-of-sums aggregation, percentiles,
  the fitter's update rule, the lognormal duty factor. Property tests
  via `proptest` where an invariant exists (a CV of 0 gives a factor
  of exactly 1, aggregation is permutation-invariant, and so on);
- every parser, against fixtures copied from real systems: kernel
  `pmu-events` JSON excerpts, sysfs PMU format files, cgroup v2 and
  resctrl file trees, `/proc/<pid>/status`;
- the fitter, driven against a simulated machine whose response to each
  knob is a known function, so convergence and damping are checked
  without hardware;
- the CLI surface, through `assert_cmd`.

Coverage is measured with `cargo llvm-cov`. Hardware-only code paths
are excluded from the headline number by the `hw-tests` feature gate,
not by `#[ignore]`, so no test is silently skipped.

## Hardware tests: `cargo test --features hw-tests`

Run on a machine with performance counters but need no privilege: they
count the test's own threads at the default `perf_event_paranoid`.
They fail, not skip, when the PMU is missing, so a passing run proves
the counters were real.

## Root tests: `sudo cargo test --features root-tests`

Need `CAP_PERFMON` or root: per-CPU and uncore counters, resctrl
groups, filter validation on AMD's L3 PMU. The feature implies
`hw-tests`. They also fail rather than skip when the privilege is
missing. Use a separate target directory so root does not own the
build artifacts the unprivileged build uses:

```sh
sudo env PATH="$PATH" CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$HOME/.rustup" \
  CARGO_TARGET_DIR=target/root cargo test --features root-tests
```

## Experiments: `experiments/`

End-to-end evidence: record a known benchmark, replay the profile,
record the replay, and plot both. Each experiment writes its raw data
and plots to `experiments/out/<name>/` and is described in
`docs/experiments.md`.
