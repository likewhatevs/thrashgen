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

Run on a machine with performance counters. Tests that need
`CAP_PERFMON`, a resctrl mount or a delegated cgroup detect what they
have and fail, not skip, when it is missing, so the invocation documents
what was proven. Run the full set with `sudo -E`.

## Experiments: `experiments/`

End-to-end evidence: record a known benchmark, replay the profile,
record the replay, and plot both. Each experiment writes its raw data
and plots to `experiments/out/<name>/` and is described in
`docs/experiments.md`.
