# Experiments

End-to-end evidence that a replay looks like the original to the same
recorder. Each experiment records a benchmark, aggregates the recording
into a profile, replays the profile with `run`, records the replay the
same way once it has had sixty percent of its time to fit, and
compares the two profiles field by field. The warm-up keeps the search
for the knobs out of the comparison; what is compared is the replay as
it holds. Both the
benchmark and the replay run in a transient user scope
(`systemd-run --user --scope`), so the recorder reads a cgroup with
the same accounting for each: busy time and memory from the cgroup
files, counters attached to the scope's threads.

The harness is `experiments/run.sh`; `experiments/all.sh` runs the set
below and draws the plots with `examples/experiment_plot.rs`. The set
takes about an hour: each benchmark records for two minutes and each
replay fits for seven, and TaoBench fills its cache for three minutes
first. `RECORD_SECS` and `REPLAY_SECS` shorten
that for a smoke run; the replay needs at least a few windows of the
length `docs/design.md` section 6 gives to converge. Nothing else
should run on the host meanwhile: a build alongside a recording shows
up in its misses and bandwidth.

`MODE` chooses the privilege the whole set runs with, and each mode
writes under its own directory, `experiments/out/<mode>/<name>/`:

| mode | what changes | why |
| --- | --- | --- |
| `user` | nothing: unprivileged, as in a container | the default view |
| `kernel` | `perf_event_paranoid` set to 1 for the run | both recordings count kernel mode, so kernel-bound work is in the ratios |
| `resctrl` | resctrl mounted, two monitoring groups chowned to the user | DRAM bandwidth measured for both, instead of modelled for the replay and absent for the original |

The two privileged modes use `sudo` for exactly those settings and
restore them on exit. Each `<name>` directory holds:

| file | what |
| --- | --- |
| `orig.jsonl`, `orig.json` | recording and profile of the benchmark |
| `report.json`, `run.log` | the replay's own report and fit log |
| `replay.jsonl`, `replay.json` | recording and profile of the replay |
| `compare.txt`, `compare.json` | field-by-field comparison |
| `ratio.svg` | replay over original per observable |
| `cores.svg` | busy cores at 1 Hz, benchmark and replay |

`experiments/out/<mode>/summary.svg` puts every experiment's ratios on
one chart. `experiments/publish.sh` copies the small files of every
mode into `docs/experiments/<mode>/`.

## The set

| name | command | what it exercises |
| --- | --- | --- |
| schbench | `schbench -m 2 -t 4 -F 4096 -n 20` | scheduler wakeups, a few MiB of cache footprint per thread, moderate IPC |
| stress-ng | `stress-ng --vm 4 --vm-bytes 1G --vm-keep --vm-method write64` | memory-bound writes, page faults, low IPC |
| sysbench | `sysbench cpu --threads=4 run` | compute-bound, high IPC, almost no misses |
| fio | `fio --rw=randread --bs=4k --size=1G --numjobs=2 --direct=0` | page cache reads, kernel-mode heavy |
| taobench | `experiments/taobench.sh`: the TaoBench server with 8 fast threads and 2 GiB of cache, driven by its client in a scope of its own | a key-value server: many threads, TLS, a cache that misses the L3 by design |

TaoBench is Meta's DCPerf benchmark of a memcached-derived server.
`experiments/install_taobench.sh` builds its server and client from
DCPerf's sources without the rest of DCPerf and installs them under
`~/.local/share/taobench`; the wrapper sizes the server like DCPerf's
own runner does, scaled down to a small container's worth of threads,
and records only the server.

## Results

Run on the Zen 4 host described in `docs/design.md`, unprivileged:
counters in user mode only, L3 misses through the DRAM-fill proxy,
bandwidth modelled and so absent from both recordings. Two minutes
recorded, seven minutes replayed, the replay recorded over its last
167 s. Plots and data are under `docs/experiments/`; `summary.svg`
puts every ratio on one chart.

| benchmark | within tolerance | busy cores | memory | IPC | L3 MPKI | what missed |
| --- | --- | --- | --- | --- | --- | --- |
| schbench | 10 of 10 | -0.2% | anon +0.2% | +1.8% | 0.0048 vs 0.0049 | nothing |
| sysbench | 10 of 10 | -0.2% | anon +0.5 MB on 1.6 MB | -3.6% | both near zero | nothing |
| stress-ng | 7 of 10 | -0.1% | anon 1.075 GB exact | 0.58 vs 0.22 | 13.9 vs 17.6 | IPC, MPKI, dTLB rate |
| fio | 5 of 10 | -5.2% | page cache 1.00 GB vs 1.03 GB | 1.31 vs 0.68 | 0.14 vs 0.24 | burstiness, IPC, MPKI, both TLB rates |

The two compute-bound benchmarks replay within tolerance on every
field. The two memory-bound ones replay their busy cores and memory
exactly and miss on the counter ratios, for reasons the design doc's
limits predict:

- **stress-ng** streams sequentially through 1 GiB with hugepages, so
  it misses the L3 at every line but almost never the TLB, and its IPC
  is that of a store stream. The generator's main stream is random: at
  one op per access and two lines per visit it reaches 14 MPKI, and at
  that point its IPC floor is 0.58 against 0.22, while its random pages
  cost one L2 TLB hit per visit against stress-ng's near zero. A
  sequential stream mode would close all three.
- **fio** spends most of its time in the kernel; the user-mode view the
  recorder gets without `CAP_PERFMON` is a thin slice of syscall
  wrappers with a high TLB rate per instruction. The replay reproduces
  the page cache and the busy cores, and its burstiness sat within a
  few percent of the target in its own windows from the eighth pass
  on, but the recorder saw a lower CV over 167 s than over the
  benchmark's full two minutes, and the user-mode ratios of a
  kernel-bound process are not something a user-mode generator can
  match.

The fitter converged in six passes on schbench; on the other three it
ran out of its twelve passes while still closing, with sysbench's IPC
within tolerance from pass ten.

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
