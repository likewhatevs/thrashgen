# Experiments

End-to-end evidence that a replay looks like the original to the same
recorder. Each experiment records a benchmark, aggregates the recording
into a profile, fits knobs to the profile with `run`, then runs the
knobs it ended on again with `--knobs` and `--no-counters`, records
that run the same way as the benchmark, and compares the two profiles
field by field. The second run keeps the search for the knobs out of
the comparison, and keeps the recorder's counters to itself: a fitting
run and a recorder attached to it share the core's six counters, and
until the scaling for the time multiplexed away was done per interval
(2026-10-18) the readings on both sides drifted apart; the disturbed
mode records a live fit, which the two now allow. Both the
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
| `crowd` | the same three profiles, four copies of each at once (`COPIES`), twelve instances | a host filled with instances: about sixty busy cores of this host's sixty-four, so the caches and the memory bus are contended for real |
| `disturbed` | each of the three profiles fitted live for 100 s while `stress-ng --vm 4` starts beside it at 35 s and is stopped at 65 s; the replay is recorded in three 25 s phases, alone, beside the neighbour and alone again, and each phase is compared with the profile | a neighbour that arrives and leaves: whether the fit follows the change, and how soon, from the report's windows |
| `perfmon` | a copy of the binary carrying the `CAP_PERFMON` file capability | what a container with `--cap-add=PERFMON` gets: kernel mode on both sides, per-cgroup counters, and L3 misses from the L3 PMU rather than the proxy, which does not count prefetched lines |

The two privileged modes use `sudo` for exactly those settings and
restore them on exit. Each `<name>` directory holds:

| file | what |
| --- | --- |
| `orig.jsonl`, `orig.json` | the recording of the benchmark, one JSON line per sample with its provenance, and the profile aggregated from it: per field the average, the percentiles where they exist, and the scope, mode and method it was measured with |
| `report.json`, `run.log`, `knobs.json` | the fitting run's report: the CPU, the thread count, every target with the tier it was checked at, its measured value and whether it was within; the knobs the run ended on; every 100 ms window with its measurements, its knobs and the pass count; and the notes, which name every fallback and every hold. The log is the same, line by line, as the run printed it; `knobs.json` is the report's knobs on their own, for the hold |
| `hold-report.json`, `hold.log` | the same for the fixed-knob run that was recorded, which has no counters and so no measured targets |
| `replay.jsonl`, `replay.json` | the recording and the profile of the replay, taken as the benchmark's were |
| `compare.txt`, `compare.json` | the two profiles field by field: expected, actual, relative error and whether it is within tolerance, as text and as rows |
| `ratio.svg` | replay over original per observable, a bar at 1.0 being a match; `docs/experiments/<mode>/<name>/ratio.svg` once published |
| `cores.svg` | busy cores at 1 Hz over the two recordings, benchmark and replay, which is where burstiness is visible |
| `phases.json`, `alone.json`, `beside.json`, `after.json`, `compare-<phase>.json` | the disturbed mode only: when the neighbour started and stopped and which seconds each phase covers, the three phases' profiles, and each one's comparison with the benchmark's |

`experiments/out/<mode>/summary.svg` puts every experiment's ratios on
one chart. `experiments/publish.sh` copies the small files of every
mode into `docs/experiments/<mode>/`, or, with `PUBLISH_DIR`, under
another host's directory, `docs/experiments/<cpu-model>/<mode>/`, so
one host's results do not overwrite another's; `experiments/plot.py`
draws the figures under `<root>/plots/` from whichever root it is
given with seaborn, inlined below. How to read them:

- **fields within** (`within.png`): per benchmark, one bar per mode
  with the fields within tolerance over the fields the recorder
  measured, the latter in grey behind it; the crowd mode's four
  copies of a profile are one bar at their mean with a line for
  their range. Tolerance is 5 percent of the target or, where that is
  wider, the field's absolute floor: 0.05 busy cores, 0.02 of
  burstiness, 0.02 IPC, 0.05 L3 MPKI, 100 MB/s of bandwidth, 0.02
  dTLB and 0.01 iTLB events per thousand instructions, 8 MiB of
  memory, as `docs/design.md` section 6 defines them.
- **error heatmap** (`errors.png`): replay less original, per field,
  in units of that field's tolerance, so every cell reads the same
  way: between -1 and +1 is within the band, +2 is twice the
  tolerance over, and a dTLB rate of 0.17 against 0.003 reads as
  eight floors over rather than thousands of percent; red is over,
  blue under, a black frame marks a field outside its band, and the
  colour is clipped at four tolerances.
- **convergence** (`convergence.png`): the second of the last knob
  move of each fit, a cross marking a fit that did not converge,
  because a target was out of reach or the fit held a compromise.
- **trajectories** (`trajectories-<mode>.png`): each fit's IPC, MPKI
  and dTLB rate over its first four seconds against the target line
  and the tolerance band, MPKI and the dTLB rate on a log scale; a
  band that fills the panel is an absolute floor wider than the
  values.
- **the disturbed mode** (`disturbed.png`): the same three ratios over
  a live fit's hundred seconds, the neighbour's presence in orange,
  the three recorded phases in grey, and each phase's fields within
  in the title.
- **per mode** (`<mode>/summary.svg`): every benchmark's ratio of
  replay to original per field, 1.0 a match, drawn by the harness
  from the comparisons.

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

What each benchmark is, and why it is in the set:

- **schbench** is the scheduler latency benchmark: message threads
  wake worker threads that spin for a while and sleep. Eight threads
  busy nearly all the time at a high IPC, a few megabytes of cache
  footprint each, almost no misses and a dTLB rate near zero. It
  stands for a compute-bound service and checks that the fit does
  not invent memory traffic where there is none.
- **stress-ng's vm stressor** runs four workers that each keep
  writing a gigabyte with 64-bit stores. Memory-bound: a miss every
  fifty instructions, an IPC of 0.22, sixteen gigabytes a second of
  streaming stores, and the page faults of a gigabyte per worker.
  It stands for the bandwidth-bound end and is where the generator's
  floors show, the dTLB floor and the misses per instruction a run
  can make.
- **sysbench's cpu test** computes primes on four threads: no
  memory, an IPC of 1.45, a cache-resident profile that checks the
  rates can be diluted to almost nothing.
- **fio** reads 4 KiB blocks at random from a gigabyte that sits in
  the page cache, two jobs with synchronous reads. Its time goes into
  the kernel's page-cache copies, so in user mode it looks like a
  bursty process of modest IPC with a high dTLB rate, and with the
  kernel counted it acquires the kernel's misses. It stands for a
  kernel-bound, bursty service, and its burstiness is what the
  thread sizing and the plateau handling were measured on.
- **TaoBench** is the memcached-derived key-value server of Meta's
  DCPerf, with its client driving it from a scope of its own; only
  the server is recorded. Eight threads, TLS, a cache that misses the
  L3 by design, a dTLB rate of six per thousand and an IPC of 0.7:
  the profile farthest from the fitter's first guess, and the one
  every lever has to work for.

## Results

Run on the Zen 4 host described in `docs/design.md`, with the 100 ms
fit windows: each replay fitted for 10 s and the knobs it ended on
then ran for 150 s with no counters of their own, recorded over the
last 135 s. The seven modes are from one chain on 2026-10-19 at
commit `33ffb22`, on a host with nothing else running, except the
`disturbed` mode, which ran again at `d393c34` after what its first
run showed of the hold for lack of progress (`docs/design.md`
section 6). An earlier chain that day had an indexer from another
session start beside it, and under `CAP_PERFMON` the L3 PMU counts
the whole L3 domain for a cgroup that may run anywhere, so a
neighbour's misses land in both recordings. "Within" counts the fields within tolerance out of the
fields the recorder measured; "fit" is the number of passes, windows
that moved a knob, and the second of the last one, or "held" when the
fit stopped on the closest pass it saw, out of passes, with a lever
frozen or no closer after six passes of re-fitting. Plots and data
are under `docs/experiments/<mode>/`, the figures under
`docs/experiments/plots/`.

![Fields within tolerance per benchmark and mode; grey is the fields measured](experiments/plots/within.png)

![Replay error by field, benchmark and mode, in units of each field's tolerance; a frame marks a field outside its band](experiments/plots/errors.png)

![Seconds to the last knob move per benchmark and mode](experiments/plots/convergence.png)

### `user`: unprivileged, as in a container

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 3 passes, 0.4 s | nothing |
| sysbench | 10 of 10 | 5 passes, 0.6 s | nothing |
| taobench | 10 of 10 | 8 passes, 7.7 s | nothing; converged at 0.7 s, then a second of MPKI 6 percent over at 7 s took two more passes |
| stress-ng | 9 of 10 | 9 passes, 1.0 s | dTLB 0.18 vs 0.0034, the hardware floor |
| fio | 10 of 10 | 7 passes, 0.8 s | nothing |

![user mode: replay over original per observable, every benchmark](experiments/user/summary.svg)

![user mode: the fits' first seconds against their targets](experiments/plots/trajectories-user.png)

### `kernel`: both recordings count kernel mode

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 3 passes, 0.4 s | nothing |
| sysbench | 10 of 10 | 5 passes, 0.6 s | nothing |
| taobench | 10 of 10 | 12 passes, 6.0 s | nothing; converged at 0.7 s, re-fitted once at 6 s |
| stress-ng | 9 of 10 | 12 passes, 2.3 s | dTLB 0.22 vs 0.009, the hardware floor |
| fio | 10 of 10 | 24 passes, 3.7 s | nothing |

![kernel mode: replay over original per observable, every benchmark](experiments/kernel/summary.svg)

![kernel mode: the fits' first seconds against their targets](experiments/plots/trajectories-kernel.png)

### `resctrl`: DRAM bandwidth measured on both sides

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 11 of 11 | 3 passes, 0.4 s | nothing; bandwidth 173 MB/s vs 124, within its floor |
| sysbench | 11 of 11 | 5 passes, 0.6 s | nothing; bandwidth 7 MB/s vs 5, within its floor |
| taobench | 11 of 11 | held, 29 passes by 5.6 s | nothing; bandwidth 9.0 vs 9.4 GB/s; a re-fit at 5 s came no closer for six passes and held pass 17 |
| stress-ng | 9 of 11 | 11 passes, 1.2 s | bandwidth 15.7 vs 69.6 GB/s, dTLB 0.18 vs 0.0035 |
| fio | 9 of 10 | held, 45 passes by 9.7 s | MPKI 0.089 vs 0.026; the replay's bandwidth had no reading |

![resctrl mode: replay over original per observable, every benchmark](experiments/resctrl/summary.svg)

![resctrl mode: the fits' first seconds against their targets](experiments/plots/trajectories-resctrl.png)

### `noisy`: three profiles at once, each in its own scope

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 9 of 10 | 10 passes, 2.7 s | IPC 3.96 vs 3.74, 6 percent over |
| taobench | 10 of 10 | 6 passes, 0.7 s | nothing |
| stress-ng | 9 of 10 | held, 27 passes by 5.3 s | dTLB 0.37 vs 0.0034, the hardware floor; a lever turned twice within three passes and the fit held pass 20, the closest |

![noisy mode: replay over original per observable, every benchmark](experiments/noisy/summary.svg)

![noisy mode: the fits' first seconds against their targets](experiments/plots/trajectories-noisy.png)

### `perfmon`: `CAP_PERFMON`, L3 misses from the L3 PMU

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 4 passes, 0.5 s | nothing |
| sysbench | 10 of 10 | 5 passes, 0.6 s | nothing |
| taobench | 10 of 10 | held, 18 passes by 5.1 s | nothing; a re-fit at 4 s came no closer for six passes and held pass 12 |
| stress-ng | 8 of 10 | held, 12 passes by 2.9 s | MPKI 43 vs 95, dTLB 4.1 vs 0.0099 |
| fio | 9 of 10 | 13 passes, 1.4 s | MPKI 21.2 vs 16.0 |

![perfmon mode: replay over original per observable, every benchmark](experiments/perfmon/summary.svg)

![perfmon mode: the fits' first seconds against their targets](experiments/plots/trajectories-perfmon.png)

### `crowd`: four copies of three profiles at once, twelve instances

| profile | copies within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 9 of 10, all four | 15 passes, 1.6 to 1.8 s | IPC 2.93 vs 3.74 on every copy |
| stress-ng | 7 to 9 of 10 | held, 14 to 36 passes by 4.0 to 8.8 s | dTLB on every copy; IPC 0.25 vs 0.22 and MPKI 16.4 vs 21.1 on one copy, MPKI 23.2 vs 21.1 on another |
| taobench | 10 of 10, all four | 6 to 10 passes, 0.7 to 1.1 s | nothing |

![crowd mode: replay over original per observable, every benchmark](experiments/crowd/summary.svg)

### `disturbed`: a live fit while stress-ng comes and goes

Each profile fitted live for 100 s; `stress-ng --vm 4 --vm-bytes 1G`
ran beside it from 35 s to 65 s. The three columns are the three 25 s
recordings: 10 to 35 s, 40 to 65 s and 70 to 95 s. "Moved" counts
the knob moves within fifteen seconds of the neighbour's arrival and
of its departure.

| profile | alone | beside | after | moved on arrival, on departure |
| --- | --- | --- | --- | --- |
| schbench | 10 of 10 | 10 of 10 | 10 of 10 | 2 passes at 1.3 s after; 1 pass at 1.3 s after |
| stress-ng | 9 of 10: dTLB floor | 8 of 10: MPKI 22.7 vs 21.1, dTLB | 9 of 10: dTLB floor | 49 passes over 11 s, a budget spent, a renewal, then the closest pass held; 10 passes from 1.1 s after, the hold renewed |
| taobench | 9 of 10: burstiness 0.10 vs 0.081 | 9 of 10: IPC 0.50 vs 0.64 | 10 of 10 | 39 passes over 10 s, two holds for lack of progress, each renewed; 26 passes from 0.9 s after |

![disturbed mode: each live fit's IPC, MPKI and dTLB rate over its hundred seconds, the neighbour's span shaded](experiments/plots/disturbed.png)

## Results on an Apple M2 Ultra

Run on 2026-10-19 at `57081ab` on the Apple M2 Ultra described in
`docs/status.md`, 16 Avalanche P cores and 8 Blizzard E cores under
Asahi Linux 7.1 with 16 KiB pages, the same harness and settings, on
a host idle but for its monitoring daemon. The host's PMU counts L3
misses on the P cores only and none a prefetcher brought in, so a
sequential writer records next to none at any bandwidth, as stress-ng
does here; there is no resctrl, so the `resctrl` mode did not run and
bandwidth is modelled on every row; and sysfs lists no L3, so the
page-cache reads are not a lever and a profile whose misses are the
kernel's copies, fio's with the kernel counted, is out of reach. The
data is under `docs/experiments/apple-m2-ultra/<mode>/`, the figures
under `docs/experiments/apple-m2-ultra/plots/`. A first chain on
2026-10-18 at `7e9fcdb`, before the access path was compiled per
geometry and the fitter judged a bursty profile at its long-run
factor, is in that commit; what moved is said under each table.

![Fields within tolerance per benchmark and mode on the M2 Ultra](experiments/apple-m2-ultra/plots/within.png)

![Replay error by field, benchmark and mode on the M2 Ultra](experiments/apple-m2-ultra/plots/errors.png)

![Seconds to the last knob move per benchmark and mode on the M2 Ultra](experiments/apple-m2-ultra/plots/convergence.png)

### `user`

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 9 of 10 | 2 passes, 0.3 s | memory 26.2 MiB vs 16.4 MiB |
| sysbench | 9 of 10 | 4 passes, 0.5 s | memory 15.1 MiB vs 5.3 MiB |
| taobench | 9 of 10 | 14 passes, 1.5 s | iTLB 1.94 vs 1.84 |
| stress-ng | 10 of 10 | 5 passes, 0.6 s | nothing |
| fio | 9 of 10 | 12 passes, 4.3 s | iTLB 1.18 vs 0.805 |

![user mode on the M2 Ultra: replay over original per observable](experiments/apple-m2-ultra/user/summary.svg)

![user mode on the M2 Ultra: the fits' first seconds against their targets](experiments/apple-m2-ultra/plots/trajectories-user.png)

Against the chain of 2026-10-18: fio went from 7 to 9 of 10, its
cores and burstiness now within, as its recording starts once its
reads are cached and reads a burstiness of 0.24 where a cold file
read 1.44, and the fit judges it at its long-run factor. schbench and
sysbench lost their memory total: the sparse and code sets now take a
quarter of the memory tolerance each, 2 MiB, which on 16 KiB pages
with the tool's own baseline puts the replay 10 MiB over a process of
16 or 5 MiB, past the 8 MiB floor; the components are within. The
iTLB rates of TaoBench and fio, 5 and 47 percent over, are the stub
call's: on this host a call's demand L1 iTLB misses read 1.0 to 1.4
from one run to the next at the same knobs, so the rate a fit lands
is not the rate its hold reads, and fio's code set of 2 MiB is 128
pages here, which the L1 iTLB holds, so each call brings a hundredth
of the events a page past the reach would and the replay makes 39
calls an access to reach the rate.

### `kernel`: both recordings count kernel mode

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 4 passes, 1.7 s | nothing |
| sysbench | 9 of 10 | 2 passes, 0.3 s | memory 11.9 MiB vs 2.9 MiB |
| taobench | 10 of 10 | 11 passes, 1.2 s | nothing |
| stress-ng | 10 of 10 | 6 passes, 0.7 s | nothing |
| fio | 8 of 10 | 32 passes, still moving at 10 s | IPC 3.04 vs 1.67, MPKI 0.032 vs 0.164 |

![kernel mode on the M2 Ultra: replay over original per observable](experiments/apple-m2-ultra/kernel/summary.svg)

![kernel mode on the M2 Ultra: the fits' first seconds against their targets](experiments/apple-m2-ultra/plots/trajectories-kernel.png)

fio's profile here has the kernel's misses from its page-cache
copies, 0.16 per thousand instructions at an IPC of 1.67, and the
replay's lever for them, page-cache reads, is not a lever on this
host, since sysfs lists no L3 to size the file against. The fit
reaches the generator's ceiling at one op and says so, was still
moving at 10 s, and the hold ran its closest pass, a compute-bound
point at an IPC of 3.0; on 2026-10-18 the hold had landed IPC and
missed the TLB rates instead. The rest is as before: schbench,
TaoBench and stress-ng within on every field, sysbench's memory total
over for the same reason as in user mode.

### `perfmon`: `CAP_PERFMON`

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 2 passes, 0.3 s | nothing |
| sysbench | 9 of 10 | 3 passes, 0.5 s | memory 15.7 MiB vs 2.8 MiB |
| taobench | 10 of 10 | 13 passes, 1.4 s | nothing |
| stress-ng | 10 of 10 | 5 passes, 0.6 s | nothing |
| fio | 5 of 10 | held, 72 passes by 8.4 s | burstiness 0.312 vs 0.335, IPC 1.41 vs 1.66, MPKI 0.0098 vs 0.123, dTLB 0.343 vs 0.387, iTLB 1.65 vs 1.03 |

![perfmon mode on the M2 Ultra: replay over original per observable](experiments/apple-m2-ultra/perfmon/summary.svg)

![perfmon mode on the M2 Ultra: the fits' first seconds against their targets](experiments/apple-m2-ultra/plots/trajectories-perfmon.png)

TaoBench is back within on its iTLB rate, 5 percent over on
2026-10-18, the same run-to-run spread the other way. fio, with the
kernel counted and no page-cache lever, held its closest pass after
36 passes, had a target it met go out, spent a renewed budget and
held again after 72: 5 of 10, where the first chain's fit was still
moving at 10 s and landed 6.

### `noisy`: three profiles at once

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench | 10 of 10 | 4 passes, 2.4 s | nothing |
| taobench | 10 of 10 | 14 passes, 1.5 s | nothing |
| stress-ng | 10 of 10 | 6 passes, 0.7 s | nothing |

![noisy mode on the M2 Ultra: replay over original per observable](experiments/apple-m2-ultra/noisy/summary.svg)

![noisy mode on the M2 Ultra: the fits' first seconds against their targets](experiments/apple-m2-ultra/plots/trajectories-noisy.png)

Every field of every profile within, where on 2026-10-18 schbench's
MPKI read 0.084 against 0.0034 beside its neighbours and its memory
total was over: the neighbours' lines in the shared system cache are
not there in every run.

### `crowd`: four copies of three profiles, twelve instances on twenty-four cores

The twelve instances ask for 61 busy cores of this host's 24, where
the Zen 4 host has 64, so the mode here fills the host two and a half
times over and every copy gets its fair share of the cores and the
burstiness of the contention.

| benchmark | within | fit | what missed |
| --- | --- | --- | --- |
| schbench-1 | 8 of 10 | 6 passes, still moving at 10 s | cores 2.81 vs 7.98, burstiness 0.168 vs 0.0007 |
| schbench-2 | 8 of 10 | 6 passes, still moving at 10 s | cores 2.86 vs 7.98, burstiness 0.222 vs 0.0007 |
| schbench-3 | 8 of 10 | 7 passes, still moving at 10 s | cores 2.69 vs 7.98, burstiness 0.143 vs 0.0007 |
| schbench-4 | 8 of 10 | 7 passes, still moving at 10 s | cores 2.96 vs 7.98, burstiness 0.151 vs 0.0007 |
| taobench-1 | 6 of 10 | held, 32 passes by 7.6 s | cores 1.28 vs 3.24, burstiness 0.281 vs 0.045, MPKI 0.942 vs 1.25, iTLB 2.30 vs 1.84 |
| taobench-2 | 6 of 10 | held, 34 passes by 9.9 s | cores 1.41 vs 3.24, burstiness 0.312 vs 0.045, MPKI 1.10 vs 1.25, iTLB 1.72 vs 1.84 |
| taobench-3 | 6 of 10 | 21 passes, still moving at 10 s | cores 1.34 vs 3.24, burstiness 0.218 vs 0.045, MPKI 1.12 vs 1.25, iTLB 1.69 vs 1.84 |
| taobench-4 | 7 of 10 | held, 29 passes by 9.7 s | cores 1.35 vs 3.24, burstiness 0.256 vs 0.045, MPKI 1.11 vs 1.25 |
| stress-ng-1 | 8 of 10 | held, 15 passes by 4.4 s | cores 1.88 vs 3.98, burstiness 0.271 vs 0.0002 |
| stress-ng-2 | 8 of 10 | 15 passes, still moving at 10 s | cores 1.92 vs 3.98, burstiness 0.260 vs 0.0002 |
| stress-ng-3 | 8 of 10 | 15 passes, still moving at 10 s | cores 1.79 vs 3.98, burstiness 0.226 vs 0.0002 |
| stress-ng-4 | 8 of 10 | 15 passes, still moving at 10 s | cores 1.59 vs 3.98, burstiness 0.259 vs 0.0002 |

![crowd mode on the M2 Ultra: replay over original per observable](experiments/apple-m2-ultra/crowd/summary.svg)

schbench's and stress-ng's copies land what they landed on
2026-10-18, the ratios at a third to a half of their cores. TaoBench's
copies went from 7 to 8 of 10 to 6 to 7: three of the four now hold
after six passes without progress, the rule the Zen 4 host added for
a fit beside a neighbour, and the pass they hold has MPKI 10 to 25
percent short and the iTLB rate out, where the first chain's fits
kept moving to 10 s and landed MPKI on three copies. On a host filled
two and a half times over what is out is out of reach at any pass,
and which pass is closest depends on which seconds of the contention
it saw.

### `disturbed`: a live fit while stress-ng comes and goes

| profile | alone | beside | after |
| --- | --- | --- | --- |
| schbench | 9 of 10: memory 26.0 MiB vs 16.4 MiB | 10 of 10 | 10 of 10 |
| taobench | 9 of 10: burstiness 0.021 vs 0.045 | 10 of 10 | 10 of 10 |
| stress-ng | 10 of 10 | 10 of 10 | 10 of 10 |

![disturbed mode on the M2 Ultra: each live fit's IPC, MPKI and dTLB rate over its hundred seconds](experiments/apple-m2-ultra/plots/disturbed.png)

As on 2026-10-18: every profile within on every field beside the
neighbour and after it, schbench's memory total and TaoBench's
burstiness a floor out in the phase alone.

What the M2 Ultra adds to the reading below:

- **The converging profiles converge as fast as on Zen 4**, schbench
  and sysbench in 0.3 to 0.5 s and TaoBench in 1.2 to 1.5 s in every
  mode, and land every ratio within in user, kernel and perfmon mode.
  stress-ng joins them here: with its sequential writes prefetched and
  the miss event counting none of them, its profile asks for an IPC
  of 1.7 to 2.0 at zero misses, which is compute, and the fit lands it
  in 0.6 to 0.7 s, every field within in every mode.
- **fio in user mode is now 9 of 10**, its cores and burstiness
  within once its recording starts from the page cache and the fit
  judges it at its long-run factor; what it misses is the iTLB rate,
  the stub call's run-to-run spread on this host and a code set the
  L1 iTLB holds. With the kernel counted it is out of reach: its
  misses are the kernel's copies and the page-cache lever needs an
  L3 size sysfs does not give.
- **Memory is the tool's own.** A process as small as sysbench, 3 to
  5 MiB, or schbench, 16 MiB, is under the replay's baseline with
  the 2 MiB stream sets on 16 KiB pages, so the memory total misses by
  more than the 8 MiB floor; the components are within.
- **Neighbours cost what they cost.** Beside two other profiles every
  field of every profile is within. With twelve instances on
  twenty-four cores every copy runs at a third to a half of its busy
  cores, and TaoBench's copies hold their closest pass under the
  contention. A neighbour arriving and leaving leaves every profile
  within on every field in the phases beside it and after.

## Reading the results

The comparison is between two recordings taken the same way, so it
measures the replay against the recorder, not against the replay's
own report. What the seven modes say:

- **Every converging profile fits in under a second.** schbench,
  sysbench, TaoBench and fio converge in 0.4 to 0.8 s in user mode
  and land every field within tolerance; TaoBench re-fits once a few
  seconds later when its MPKI drifts to the edge of its band and is
  back within in two passes. stress-ng reaches everything but its
  dTLB rate by its ninth pass, 1.0 s.
- **stress-ng meets everything but its dTLB rate**, which is a floor
  of this host's hardware: under the L2-hit definition the L1 dTLB
  reloads on about 1.4 percent of hugepage touches whatever the set,
  and a memory-bound target at low IPC needs dependent misses, each
  in a page of its own. A sequential walk would meet the rate and
  lift IPC to 0.74 against 0.22. Its 70 GB/s is a streaming store's
  bandwidth, several times what its counted misses move; random
  64-byte lines reach 16 GB/s here. See `docs/design.md` section 5.1
  and `docs/status.md`.
- **fio lands in every mode where its misses are user-mode or the
  kernel's alike.** Its recording now starts once its reads run from
  the page cache, so it is a steady process at a quarter of a core
  with a per-second CV near 0.2, and its rates per instruction still
  differ between a busy second and a quiet one by the refetch each
  wakeup costs. The fitter learns that cost from two plateaus and
  judges every window at the long-run factor, so the fit lands in 7
  passes in user mode and holds; with the kernel in the count the
  page-cache reads supply the kernel's misses and the fit lands in 24
  passes, 3.7 s, every field within. Under resctrl its MPKI, 0.09
  against 0.03, is the one field out, and the replay's bandwidth has
  no reading, since the monitoring counters lose a single
  low-duty thread (`docs/status.md`, open item 1); under the L3 PMU
  its MPKI lands a third over.
- **The L3 PMU changes the question.** With `CAP_PERFMON` the
  recorder counts every L3 miss, prefetched lines included, so
  stress-ng's profile reads 95 misses per thousand instructions where
  the proxy saw 21. A miss every ten instructions needs the prefetched
  walk, which lifts IPC as it lifts MPKI; the fitter sees the two
  rules pull a lever against each other, freezes it and holds the
  closest pass as the compromise by 2.9 s, 43 MPKI at the target's
  IPC. The L3 PMU also counts the whole L3 domain for a cgroup that
  may run anywhere, so this mode needs a quiet host: an earlier chain
  that day had an indexer from another session start beside it, and
  sysbench's recording read 5.7 MPKI where it reads 0.05 alone.
- **Measured bandwidth** lands within tolerance for every profile
  that can reach it: within the floor for the compute-bound profiles,
  4 percent under for TaoBench, whose re-fit at 5 s came no closer for
  six passes and held its closest pass. stress-ng's 70 GB/s is beyond
  what its counted misses move. Where bandwidth is modelled, for want
  of resctrl, it is reported and not fitted.
- **Neighbours cost what the host costs.** With three replays sharing
  the caches and the memory bus, TaoBench lands 10 of 10 in 0.7 s and
  stress-ng meets everything but its floor; schbench's IPC lands 6
  percent over. With twelve instances on sixty-four hardware threads,
  schbench's replays sit at IPC 2.93 against 3.74 on every copy: eight
  threads at that IPC need cores to themselves, and with sixty busy
  threads each shares a core with a sibling. TaoBench's four copies
  land every field; stress-ng's four want sixteen gigabytes a second
  each from a bus the other eight instances use too, and their fits
  hold a compromise by four to nine seconds, within on seven to nine
  fields. Memory is within on every instance, now that the generator
  measures its own cgroup.
- **A neighbour arriving moves the fit within about a second, and a
  neighbour leaving within about a second too.** schbench moves twice
  on arrival and once on departure and is within on every field in
  every phase. Beside the memory-bound neighbour, what the replay
  cannot have is what the neighbour takes: TaoBench's IPC lands at
  0.50 against 0.64 and stress-ng's MPKI 8 percent over, and both are
  back within on every field they can reach once it has left. The
  re-fits are bounded now: a re-fit that comes no closer for six
  passes, five seconds in, holds its closest pass, and a hold whose
  targets go out, or go much further out, renews; stress-ng spent one
  budget and one renewal on the arrival, 49 passes over 11 s, where
  it used to spend two budgets and never renew on departure.

What the plots cannot show is anything outside the observables: the
replay has no syscalls, no I/O and no wakeups, however close the
numbers are.
