# Overview

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
fails for lack of a counter.

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
the same CPU model starts from it.

![Fields within tolerance per benchmark and privilege mode on the reference host](experiments/plots/within.png)

The chapters:

- [Working here](agents.md): where work on the code starts, the
  goals, the rules and the method
- [Design](design.md): the contract, every observable and knob and
  what the hardware cannot do
- [Architecture](architecture.md): the data flow, the modules, the
  run loop and the fitter's pass, as diagrams
- [Privileges](privileges.md): what each privilege level buys and how
  measurement degrades without it
- [Testing](testing.md): the three layers of tests and how to run
  them
- [Experiments](experiments.md): the benchmark replays in every mode,
  their data and figures
- [Status](status.md): what is verified, as of when, and what is open

The source is at <https://github.com/likewhatevs/thrashgen>, under
the GNU General Public License, version 2 only. Implemented for Linux
on x86-64, with event tables for AMD Zen 3 to 5, Intel Skylake-SP
through Granite Rapids and Sierra Forest, and Arm Neoverse; the
AArch64 paths compile for that target and have not run on it.
