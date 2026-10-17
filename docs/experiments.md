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

Run on the Zen 4 host described in `docs/design.md` on 2026-10-17,
all five modes from one fitter, with the 100 ms fit windows: each
replay fitted for 10 s and the knobs it ended on then ran for 150 s
with no counters of their own, recorded over the last 135 s. "Within"
counts the fields within tolerance out of the fields the recorder
measured; "fit" is the number of passes, windows that moved a knob,
and the second of the last one, or "held" when the fit ran out of its
thirty-six passes without converging and held the closest pass it
saw. Plots and data are under `docs/experiments/<mode>/`.

### `user`: unprivileged, as in a container

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 3 passes, 0.4 s | nothing |
| sysbench | 10 of 10 | 5 passes, 0.6 s | nothing |
| taobench | 10 of 10 | 10 passes, 1.1 s | nothing |
| stress-ng | 9 of 10 | 7 passes, 0.8 s | dTLB 0.19 vs 0.0034 |
| fio | 9 of 10 | 11 passes, 1.2 s | burstiness 0.48 vs 0.66 |

### `kernel`: both recordings count kernel mode

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 2 passes, 0.3 s | nothing |
| sysbench | 10 of 10 | 6 passes, 1.8 s | nothing |
| taobench | 10 of 10 | 10 passes, 1.2 s | nothing |
| stress-ng | 9 of 10 | 7 passes, 0.8 s | dTLB 0.25 vs 0.019 |
| fio | 7 of 10 | held, 36 passes by 4.2 s | burstiness, IPC 1.58 vs 1.39, MPKI 0.90 vs 2.43 |

### `resctrl`: DRAM bandwidth measured on both sides

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
| schbench | 10 of 10 | 6 passes, 4.9 s | nothing |
| taobench | 9 of 10 | 9 passes, 1.0 s | IPC 0.57 vs 0.61 |
| stress-ng | 7 of 10 | 8 passes, 0.9 s | IPC 0.18 vs 0.22, MPKI 25.6 vs 21.2, dTLB |

### `perfmon`: `CAP_PERFMON`, L3 misses from the L3 PMU

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 4 passes, 0.5 s | nothing |
| sysbench | 10 of 10 | 5 passes, 0.6 s | nothing |
| taobench | 9 of 10 | 12 passes, 9.2 s | iTLB 5 percent high |
| stress-ng | 7 of 10 | held, 36 passes by 3.8 s | IPC 0.27 vs 0.22, MPKI 50 vs 95, dTLB |
| fio | 5 of 10 | held, 36 passes by 4.5 s | burstiness, IPC 1.53 vs 1.39, MPKI 5.2 vs 15.6, dTLB 5.5 vs 6.2, iTLB |

## Reading the results

The comparison is between two recordings taken the same way, so it
measures the replay against the recorder, not against the replay's
own report. What the five modes say:

- **Every profile but one fits in about a second.** The compute-bound
  profiles converge in 0.3 to 1.8 s in every mode, with two
  neighbours or without; TaoBench, the farthest from the first guess,
  converges in ten passes, 1.1 s, and lands every field within
  tolerance in the user, kernel and resctrl modes, bandwidth
  included.
- **stress-ng meets everything but its dTLB rate**, which is a floor
  of this host's hardware: under the L2-hit definition the L1 dTLB
  reloads on about 1.4 percent of hugepage touches whatever the set,
  and a memory-bound target at low IPC needs dependent misses, each
  in a page of its own. A sequential walk would meet the rate and
  lift IPC to 0.74 against 0.22. Its 70 GB/s is a streaming store's
  bandwidth; random 64-byte lines reach 15 GB/s here. See
  `docs/design.md` section 5.1 and `docs/status.md`.
- **fio is the user-mode view of a kernel-bound process.** In user
  mode nine fields are within; its burstiness is the model's, since
  the fit is shorter than the thirty seconds the per-second CV needs.
  In kernel mode the profile acquires the kernel's own misses, 2.4
  per thousand instructions from page-cache copies, which a
  user-mode generator over a 2 MiB anonymous footprint makes only as
  refills after an idle period; its 100 ms windows then swing between
  cold and warm and the fit does not settle. In resctrl mode the
  profile's 844 MB/s is the same page-cache traffic; at fio's
  instructions per access the non-temporal lines deliver some 150
  MB/s at their cap.
- **The L3 PMU changes the question.** With `CAP_PERFMON` the
  recorder counts every L3 miss, prefetched lines included, so
  stress-ng's profile reads 95 misses per thousand instructions where
  the proxy saw 21, and fio's 15.6 where it saw 2.4. The fitter does
  reach for the prefetched walk, which is the only way to a miss every
  ten instructions, but the walk lifts IPC as it lifts MPKI, the two
  rules trade the sequential share and the run length back and forth,
  and both fits hold their closest pass. The compute-bound profiles
  and TaoBench are unaffected.
- **Measured bandwidth** agrees with the model for the compute-bound
  profiles and for TaoBench, 9.7 against 9.8 GB/s, once the rule
  waits for IPC and MPKI to be near their targets.
- **Neighbours cost little while the fit runs and more while it does
  not**: with three replays sharing the caches and the memory bus,
  schbench and TaoBench land 10 and 9 of 10. stress-ng's IPC and MPKI
  fit within tolerance at 0.9 s and drifted 20 percent during the
  hold, since the neighbours' own loads changed between its fit and
  its counter-free hold; a live fit, which moves again after a second
  out of tolerance, follows that, and the hold cannot.

What the plots cannot show is anything outside the observables: the
replay has no syscalls, no I/O and no wakeups, however close the
numbers are.
