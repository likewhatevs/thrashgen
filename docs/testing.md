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
- the CLI surface, through `assert_cmd`; the tests that generate load
  run one at a time (`serial_test`), since two fits on one machine
  would measure each other.

Coverage is measured with `cargo llvm-cov`; the unit, CLI and plot
tests together cover 96% of lines and 95% of regions as of the
experiment results in `docs/experiments.md`. Hardware-only code paths
are excluded from that number by the `hw-tests` feature gate, not by
`#[ignore]`, so no test is silently skipped.

## Hardware tests: `cargo test --release --features hw-tests`

Run on a machine with performance counters but need no privilege: they
count the test's own threads at the default `perf_event_paranoid`.
They fail, not skip, when the PMU is missing, so a passing run proves
the counters were real. The fit test needs the optimized binary, as
the experiments run it: an unoptimized access costs some fifteen times
the instructions, and no knob reaches the miss rate it asks for; under
the debug profile it fails and says so.

## Root tests: build as the user, run the binary as root

Need `CAP_PERFMON` or root: per-CPU and uncore counters, resctrl
groups, filter validation on AMD's L3 PMU. The feature implies
`hw-tests`. They also fail rather than skip when the privilege is
missing. Build the test binary unprivileged, then run only the root
tests from it as root, one at a time, since two of them share an L3
and would see each other's misses:

```sh
sudo mount -t resctrl resctrl /sys/fs/resctrl
bin=$(cargo test --features root-tests --no-run 2>&1 | sed -n 's/.*(\(target[^)]*thrashgen-[^)]*\))/\1/p')
sudo "$bin" root_tests --test-threads=1
sudo umount /sys/fs/resctrl
```

Compiling as root is avoided on purpose: it leaves root-owned
artifacts behind and, on this host, rustc under sudo could not write
its dependency files at all.

## Experiments: `experiments/all.sh`

End-to-end evidence: record a known benchmark, replay the profile,
record the replay the same way, and plot both. Each experiment writes
its raw data and plots to `experiments/out/<mode>/<name>/` and is
described in `docs/experiments.md`. The per-experiment plotting code is
covered by `tests/plot.rs` against fixture recordings, so a broken
chart fails `cargo test`; the figures across modes come from
`experiments/plot.py`, which needs Python with seaborn, pandas and
matplotlib and is run by hand after `experiments/publish.sh`.
