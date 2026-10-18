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
each replay fits for ten seconds and holds for two and a half
minutes, and TaoBench fills its cache for three minutes first. `FIT_SECS` and `HOLD_SECS` change
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
| `perfmon` | a copy of the binary carrying the `CAP_PERFMON` file capability | what a container with `--cap-add=PERFMON` gets: kernel mode on both sides, per-cgroup counters, and L3 misses from the L3 PMU rather than the proxy, which does not count prefetched lines |

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
mode into `docs/experiments/<mode>/`, and `experiments/plot.py` draws
the figures under `docs/experiments/plots/` from them with seaborn:
the error of every field in every mode as a heatmap, the fields within
tolerance, the seconds to the last knob move, and the first seconds of
the user-mode fits against their targets.

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

Run on the Zen 4 host described in `docs/design.md`, with the 100 ms
fit windows: each replay fitted for 10 s and the knobs it ended on
then ran for 150 s with no counters of their own, recorded over the
last 135 s. The user, kernel, noisy and perfmon modes are from
2026-10-18 and the fitter at commit `cf1436d`; the resctrl mode is
from 2026-10-17 and the fitter of that day, since on 2026-10-18 the
host could not mount resctrl: a container started while it was
mounted kept the mount in its own namespace (`docs/status.md`, open
item 1). "Within" counts the fields within tolerance out of the
fields the recorder measured; "fit" is the number of passes, windows
that moved a knob, and the second of the last one, or "held" when the
fit stopped on the closest pass it saw, out of passes or with a lever
frozen. Plots and data are under `docs/experiments/<mode>/`, the
figures under `docs/experiments/plots/`.

### `user`: unprivileged, as in a container

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 2 passes, 0.3 s | nothing |
| sysbench | 10 of 10 | 5 passes, 0.6 s | nothing |
| taobench | 10 of 10 | 6 passes, 0.7 s | nothing |
| stress-ng | 9 of 10 | 9 passes, 1.0 s | dTLB 0.17 vs 0.0058 |
| fio | 9 of 10 | 13 passes, 1.4 s | iTLB 0.070 vs 0.025 |

### `kernel`: both recordings count kernel mode

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 2 passes, 0.3 s | nothing |
| sysbench | 10 of 10 | 5 passes, 0.6 s | nothing |
| taobench | 10 of 10 | 8 passes, 4.4 s | nothing |
| stress-ng | 9 of 10 | 9 passes, 1.0 s | dTLB 0.22 vs 0.011 |
| fio | 7 of 10 | 45 passes, 8.3 s | IPC 2.12 vs 1.42, MPKI 0.39 vs 2.17, iTLB 0.010 vs 0.063 |

### `resctrl`: DRAM bandwidth measured on both sides (2026-10-17)

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 11 of 11 | 2 passes, 0.3 s | nothing; bandwidth 175 MB/s vs 120, within its floor |
| sysbench | 11 of 11 | 5 passes, 0.6 s | nothing; bandwidth within its floor |
| taobench | 11 of 11 | 10 passes, 1.1 s | nothing; bandwidth 9.7 vs 9.8 GB/s |
| stress-ng | 9 of 11 | 8 passes, 3.7 s | dTLB, bandwidth 15 vs 70 GB/s |
| fio | 6 of 10 | held, 36 passes by 9.3 s | burstiness, IPC 1.09 vs 0.93, dTLB 8.0 vs 8.6, iTLB 0.010 vs 0.027; the replay's bandwidth had no reading |

### `noisy`: three profiles at once, each in its own scope

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 5 passes, 0.6 s | nothing |
| taobench | 10 of 10 | 6 passes, 0.7 s | nothing |
| stress-ng | 9 of 10 | 15 passes, 3.8 s | dTLB 0.19 vs 0.0058 |

### `perfmon`: `CAP_PERFMON`, L3 misses from the L3 PMU

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 4 passes, 0.5 s | nothing |
| sysbench | 10 of 10 | 5 passes, 0.6 s | nothing |
| taobench | 10 of 10 | 5 passes, 0.6 s | nothing |
| stress-ng | 8 of 10 | held, 9 passes by 1.2 s | MPKI 38 vs 95, dTLB |
| fio | 6 of 10 | held, 36 passes by 3.9 s | burstiness 1.1 vs 1.6, MPKI 17.6 vs 15.2, dTLB 5.4 vs 6.2, iTLB 0.24 vs 0.056 |

## Reading the results

The comparison is between two recordings taken the same way, so it
measures the replay against the recorder, not against the replay's
own report. What the five modes say:

- **Every converging profile fits in under a second.** schbench,
  sysbench and TaoBench converge in 0.3 to 0.7 s in every mode, with
  two neighbours or without, and land every field within tolerance;
  TaoBench, the farthest from the first guess, takes six passes where
  it took ten the day before, since the sparse visits now correct by
  the dTLB events they are short rather than by ratio. stress-ng
  reaches everything but its dTLB rate by its tenth pass, 1.0 s.
- **stress-ng meets everything but its dTLB rate**, which is a floor
  of this host's hardware: under the L2-hit definition the L1 dTLB
  reloads on about 1.4 percent of hugepage touches whatever the set,
  and a memory-bound target at low IPC needs dependent misses, each
  in a page of its own. A sequential walk would meet the rate and
  lift IPC to 0.74 against 0.22. Its 70 GB/s is a streaming store's
  bandwidth, several times what its counted misses move; random
  64-byte lines reach 15 GB/s here. The fit never converges, since
  the dTLB rate is always out, so it stays bound by its pass budget
  and nudges when a pooled MPKI crosses the edge of its band, which is
  the 15 passes with neighbours. See `docs/design.md` section 5.1 and
  `docs/status.md`.
- **fio is a bursty, kernel-bound process seen one plateau at a
  time.** Its duty cycle holds each of twenty factors for four
  seconds, and its rates per instruction differ several times over
  between a quiet plateau, where every wakeup refetches a cold cache
  and TLB, and a busy one. In user mode the fit converged in 1.4 s
  inside one plateau and nine fields held over the 135 s hold; the
  iTLB rate, which the stub calls set for that plateau, averaged
  three times higher over all of them. In kernel mode the profile
  acquires the kernel's own misses, 2.2 per thousand instructions
  from page-cache copies, and the fit re-fits at each plateau and
  does not settle: 45 passes, its last at 8.3 s, and the hold ran at
  the IPC of the plateau it ended in. In resctrl mode the replay's
  bandwidth had no reading.
- **The L3 PMU changes the question.** With `CAP_PERFMON` the
  recorder counts every L3 miss, prefetched lines included, so
  stress-ng's profile reads 95 misses per thousand instructions where
  the proxy saw 21, and fio's 15 where it saw 2. A miss every ten
  instructions needs the prefetched walk, which lifts IPC as it lifts
  MPKI; the fitter sees the two rules pull the run length against each
  other, freezes it by the ninth pass and holds the closest pass as
  the compromise, 38 MPKI at the target's IPC, where it used to cycle
  through its whole budget. The compute-bound profiles and TaoBench
  are unaffected: TaoBench's 20 MPKI under the L3 PMU is met in five
  passes.
- **Measured bandwidth** agrees with the model for the compute-bound
  profiles and for TaoBench, 9.7 against 9.8 GB/s, once the rule
  waits for IPC and MPKI to be near their targets. Where bandwidth is
  modelled, for want of resctrl, it is reported and not fitted since
  2026-10-18.
- **Neighbours cost nothing measurable here.** With three replays
  sharing the caches and the memory bus, schbench and TaoBench land 10
  of 10 in 0.6 and 0.7 s, and stress-ng's IPC and MPKI land within one
  percent over the hold, where the day before they had drifted 20
  percent: the run-length rule no longer chases a pooled MPKI short
  by noise to its cap.

What the plots cannot show is anything outside the observables: the
replay has no syscalls, no I/O and no wakeups, however close the
numbers are.
