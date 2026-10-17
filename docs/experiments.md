# Experiments

End-to-end evidence that a replay looks like the original to the same
recorder. Each experiment records a benchmark, aggregates the recording
into a profile, fits knobs to the profile with `run`, then runs the
knobs it ended on again with `--knobs` and `--no-counters`, records
that run the same way as the benchmark, and compares the two profiles
field by field. The second run keeps the search for the knobs out of
the comparison, and keeps the recorder's counters to itself: a fitting
run and a recorder attached to it share the core's six counters, and
the time-multiplexed readings on both sides drifted apart. Both the
benchmark and the replay run in a transient user scope
(`systemd-run --user --scope`), so the recorder reads a cgroup with
the same accounting for each: busy time and memory from the cgroup
files, counters attached to the scope's threads.

The harness is `experiments/run.sh`; `experiments/all.sh` runs the set
below and draws the plots with `examples/experiment_plot.rs`. The set
takes about half an hour: each benchmark records for two minutes,
each replay fits for three quarters of a minute and holds for two and
a half, and TaoBench fills its cache for three minutes first. `FIT_SECS` and `HOLD_SECS` change
the two replay lengths, and `RECORD_SECS` the recording; a smoke run
can shorten all three, but the fit needs at least a few windows of
the length `docs/design.md` section 6 gives to converge. Nothing else
should run on the host meanwhile: a build alongside a recording shows
up in its misses and bandwidth.

`MODE` chooses the privilege the whole set runs with, and each mode
writes under its own directory, `experiments/out/<mode>/<name>/`:

| mode | what changes | why |
| --- | --- | --- |
| `user` | nothing: unprivileged, as in a container | the default view |
| `kernel` | `perf_event_paranoid` set to 1 for the run | both recordings count kernel mode, so kernel-bound work is in the ratios |
| `resctrl` | resctrl mounted, two monitoring groups chowned to the user | DRAM bandwidth measured for both, instead of modelled for the replay and absent for the original |
| `noisy` | the schbench, stress-ng and TaoBench profiles of a finished `user` set, fitted and held at the same time, each in its own scope | several containers on one host: each instance measures only its own scope and sees the others through the caches and the memory bus |

The two privileged modes use `sudo` for exactly those settings and
restore them on exit. Each `<name>` directory holds:

| file | what |
| --- | --- |
| `orig.jsonl`, `orig.json` | recording and profile of the benchmark |
| `report.json`, `run.log`, `knobs.json` | the fitting run's report and log, and the knobs it ended on |
| `hold-report.json`, `hold.log` | the fixed-knob run that was recorded |
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

Run on the Zen 4 host described in `docs/design.md` on 2026-10-16,
all four modes from one commit, with the thirty-second fit: each
replay fitted for 45 s and the knobs it ended on then ran for 150 s
with no counters of their own, recorded over the last 135 s. "Within"
counts the fields within tolerance out of the fields the recorder
measured; "fit" is the pass and second at which the fit converged, or
"held" when it ran out of its fifteen passes and held what it had.
Plots and data are under `docs/experiments/<mode>/`.

### `user`: unprivileged, as in a container

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 13 passes, 28 s | nothing |
| sysbench | 10 of 10 | 8 passes, 18 s | nothing |
| taobench | 8 of 10 | held | IPC 0.67 vs 0.75, MPKI 5.3 vs 5.6 |
| stress-ng | 8 of 10 | held | MPKI 13.6 vs 20.7, dTLB 0.031 vs 0.0035 |
| fio | 8 of 10 | held | burstiness 0.68 vs 0.87, dTLB 5.1 vs 8.7 |

### `kernel`: both recordings count kernel mode

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 3 passes, 8 s | nothing |
| sysbench | 10 of 10 | 8 passes, 18 s | nothing |
| taobench | 9 of 10 | held | MPKI 3.6 vs 3.9 |
| stress-ng | 8 of 10 | held | MPKI 21.5 vs 20.3, dTLB 0.36 vs 0.012 |
| fio | 5 of 10 | held | burstiness, IPC 1.54 vs 1.41, MPKI 1.1 vs 2.2, dTLB 14 vs 6.1, iTLB |

### `resctrl`: DRAM bandwidth measured on both sides

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 11 of 11 | 13 passes, 28 s | nothing; bandwidth 118 MB/s vs 104 |
| sysbench | 11 of 11 | 13 passes, 28 s | nothing; bandwidth within its floor |
| taobench | 9 of 11 | held | IPC 0.87 vs 0.68, bandwidth 11.1 vs 9.6 GB/s |
| stress-ng | 7 of 11 | held | IPC 0.24 vs 0.22, MPKI 13 vs 21, dTLB, bandwidth 10 vs 70 GB/s |
| fio | 6 of 10 | held | busy cores 0.38 vs 0.27, IPC 1.17 vs 0.83, burstiness, iTLB; the replay's bandwidth had no reading |

### `noisy`: three profiles at once, each in its own scope

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 5 passes, 12 s | nothing |
| taobench | 9 of 10 | held | IPC 0.68 vs 0.75 |
| stress-ng | 8 of 10 | held | MPKI 22.2 vs 20.7, dTLB 0.12 vs 0.0035 |

## Reading the results

The comparison is between two recordings taken the same way, so it
measures the replay against the recorder, not against the replay's
own report. What the four modes say:

- **The compute-bound profiles converge inside the budget in every
  mode**, in 8 to 28 seconds, with two neighbours or without, and
  their busy cores, memory and ratios land within tolerance.
- **TaoBench converges in some runs and holds a few percent off in
  others.** Two earlier runs on the same fitter reached 10 of 10 in
  14 passes; at the fifteenth pass of these runs its IPC sat 3 to 10
  percent off. The budget is the trade: a profile this far from the
  first guess needs most of the fifteen passes, and a window or two
  of noise at the end decides. A calibration from an earlier run
  (`--calibration`) starts it where it ended and takes the question
  away.
- **stress-ng's IPC is now within tolerance in every mode**, which
  the dependent visits bought; its MPKI varies from 7 percent high to
  a third low between runs, since the run length that gives it is
  still moving at the fifteenth pass, and its dTLB rate cannot be
  met: a random stream over a hugepage set small enough for the
  first-level TLB keeps missing the L3 only above the cache's size,
  and this host's window between the two is where the set already
  sits. Its 70 GB/s is a streaming store's bandwidth; random 64-byte
  lines reach 10 to 18 GB/s here.
- **fio is the user-mode view of a kernel-bound process.** In user
  mode its busy cores, IPC and MPKI are now within tolerance; its
  dTLB rate is short because the sparse page set is capped at an
  eighth of a 3.4 MB anonymous footprint, about a hundred pages, which
  the first-level TLB mostly holds; its burstiness is short because an
  access of half a million instructions has a coarse duty cycle. In
  kernel mode the profile acquires the kernel's own misses and TLB
  events, which a user-mode generator does not produce.
- **Measured bandwidth** agrees with the model for the compute-bound
  profiles. For the memory-bound ones the bandwidth rule's levers,
  the write share and non-temporal lines, move IPC as well, and with
  the budget spent on them taobench's IPC drifted 28 percent high; the
  rule now moves only while IPC and MPKI are near their targets, and
  stays an open item in `docs/status.md`.
- **Neighbours cost little**: with three replays sharing the caches
  and the memory bus, the compute-bound one converged in 12 seconds,
  taobench held within 9 percent on IPC and 2 on MPKI, and stress-ng
  within 7 percent on MPKI, each instance measuring only its own
  scope.

What the plots cannot show is anything outside the observables: the
replay has no syscalls, no I/O and no wakeups, however close the
numbers are.
