# thrashgen

Record what a container, process or host does to its machine, then
generate synthetic load that does the same thing.

The footprint is a small set of observables: busy cores and how bursty
they are, instructions per cycle, last-level cache misses, DRAM
bandwidth, TLB pressure, and the split of memory between anonymous,
page cache and shared. `record` samples them from a cgroup, a process,
a command it launches, or the whole host. `aggregate` turns one or
more recordings into a profile with averages and percentiles. `run`
takes a profile, generates load, measures itself the same way the
recorder did, and reports how close it got to every target.

Every measurement that needs a privilege the process lacks degrades to
the next best source and is labelled as such in the report. Nothing
fails for lack of a counter. See `docs/design.md` for the observables
and their definitions, `docs/architecture.md` for how the pieces fit,
`docs/privileges.md` for what each privilege level buys, and
`docs/testing.md` for how correctness is checked.

```sh
thrashgen caps                                   # what this process can measure
thrashgen record --cgroup system.slice/app.service --duration 3600 -o app.jsonl
thrashgen aggregate app.jsonl -o app.json
thrashgen run app.json --pick p80 --duration 600 --report replay.json
thrashgen compare app.json replay.json --pick p80
thrashgen run --cpu-usage-cores 2 --ipc 1.2 --l3-mpki 5 --duration 300   # targets without a profile
```

`run` fits its knobs in a few seconds, a tenth of a second a pass and
thirty-six passes at most, until every ratio it can measure is within
tolerance, then holds; busy cores and burstiness are set from a model
at the first window and judged from the third second on, each second
against what the model intended for it. A fit that converged is kept
in the user's cache directory, so the next run of the same profile on
the same CPU model starts from it. `calibrate` does the same and saves
the knobs for the CPU model, so a host without counters can start from
them. Hardware-dependent behaviour is checked by the tests
and experiments described in `docs/testing.md`; `docs/experiments.md`
describes the benchmark comparisons and their plots.

Implemented for Linux on x86-64, with event tables for AMD Zen 3 to 5,
Intel Skylake-SP through Granite Rapids and Sierra Forest, and Arm
Neoverse. The AArch64 paths, the code stream's stubs and the
non-temporal stores, compile for that target and have not run on it;
on other architectures the run fails at the code stream.
`docs/status.md` lists what is verified and what is open, and
`AGENTS.md` is where work on the code starts.

Licensed under the GNU General Public License, version 2 only; see
`LICENSE`.
