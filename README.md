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
and their definitions, `docs/privileges.md` for what each privilege
level buys, and `docs/testing.md` for how correctness is checked.

```sh
thrashgen caps                                   # what this process can measure
thrashgen record --cgroup system.slice/app.service --duration 3600 -o app.jsonl
thrashgen aggregate app.jsonl -o app.json
thrashgen run app.json --pick p80 --duration 600 --report replay.json
thrashgen compare app.json replay.json --pick p80
```

`run` fits its knobs window by window until every target it can
measure is within tolerance, then holds. `calibrate` does the same and
saves the knobs for the CPU model, so a host without counters can
start from them. Hardware-dependent behaviour is checked by the tests
and experiments described in `docs/testing.md`; `docs/experiments.md`
describes the benchmark comparisons and their plots.

Implemented for Linux on x86_64, with event tables for AMD Zen 3 to 5,
Intel Skylake-SP through Granite Rapids and Sierra Forest, and Arm
Neoverse; the code stream (iTLB) is x86_64 only and other
architectures report it as unavailable.
