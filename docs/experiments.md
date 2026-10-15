# Experiments

End-to-end evidence that a replay looks like the original to the same
recorder. Each experiment records a benchmark, aggregates the recording
into a profile, replays the profile with `run`, records the replay the
same way, and compares the two profiles field by field. Both the
benchmark and the replay run in a transient user scope
(`systemd-run --user --scope`), so the recorder reads a cgroup with
the same accounting for each: busy time and memory from the cgroup
files, counters attached to the scope's threads.

The harness is `experiments/run.sh`; `experiments/all.sh` runs the set
below and draws the plots with `examples/experiment_plot.rs`. The set
takes about forty minutes: each benchmark records for two minutes and
each replay fits for seven. `RECORD_SECS` and `REPLAY_SECS` shorten
that for a smoke run; the replay needs at least a few windows of the
length `docs/design.md` section 6 gives to converge. Outputs
land in `experiments/out/<name>/`:

| file | what |
| --- | --- |
| `orig.jsonl`, `orig.json` | recording and profile of the benchmark |
| `report.json`, `run.log` | the replay's own report and fit log |
| `replay.jsonl`, `replay.json` | recording and profile of the replay |
| `compare.txt`, `compare.json` | field-by-field comparison |
| `ratio.svg` | replay over original per observable |
| `cores.svg` | busy cores at 1 Hz, benchmark and replay |

`experiments/out/summary.svg` puts every experiment's ratios on one
chart.

## The set

| name | command | what it exercises |
| --- | --- | --- |
| schbench | `schbench -m 2 -t 4 -F 4096 -n 20` | scheduler wakeups, a few MiB of cache footprint per thread, moderate IPC |
| stress-ng | `stress-ng --vm 4 --vm-bytes 1G --vm-keep --vm-method write64` | memory-bound writes, page faults, low IPC |
| sysbench | `sysbench cpu --threads=4 run` | compute-bound, high IPC, almost no misses |
| fio | `fio --rw=randread --bs=4k --size=1G --numjobs=2 --direct=0` | page cache reads, kernel-mode heavy |

TaoBench from Meta's DCPerf is not in the set: it needs its own
client/server harness and is not packaged on this host. The harness
takes any command, so adding it is one line once it is installed.

## Reading the results

The comparison is between two recordings taken the same way, so it
measures the replay against the recorder, not against the replay's own
report. Expect:

- busy cores, memory composition and the counter ratios within the
  tolerances in `docs/design.md`, when the replay converged;
- kernel-mode work absent from the replay: the recorder runs without
  `CAP_PERFMON` here, so both recordings count user mode only, and the
  fio experiment's instructions are mostly kernel-side to begin with;
- DRAM bandwidth modelled, not measured, for the same reason, and
  absent from the recordings.

What the plots cannot show is anything outside the observables: the
replay has no syscalls, no I/O and no wakeups, however close the
numbers are.
