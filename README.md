# thrashgen

Record what a container, process or host does to its machine, then
generate synthetic load that does the same thing.

A footprint is a small set of observables: busy cores and how bursty
they are, instructions per cycle, last-level cache misses, DRAM
bandwidth, TLB pressure, and memory split into anonymous, page cache
and shared. `record` samples them from a cgroup, a process, a command
or the whole host. `aggregate` turns recordings into a profile. `run`
generates load fitted to the profile, measures itself the way the
recorder did, and reports how close it got to every target.

## Use

```sh
thrashgen caps                                   # what this process can measure
thrashgen record --cgroup system.slice/app.service --duration 3600 -o app.jsonl
thrashgen aggregate app.jsonl -o app.json
thrashgen run app.json --pick p80 --duration 600 --report replay.json
thrashgen compare app.json replay.json --pick p80
thrashgen run --cpu-usage-cores 2 --ipc 1.2 --l3-mpki 5 --duration 300   # targets without a profile
```

A fit lands in about a second on a cold start and then holds. It
moves again when the host changes around it, after a second out of
tolerance, and a repeat run of the same profile on the same CPU model
starts from the cached fit.

## What to expect

- Every measurement that needs a privilege the process lacks degrades
  to the next best source, and the report says which. Nothing fails
  for lack of a counter.
- On the reference host, an AMD Zen 4, the benchmark replays land
  every field the hardware allows within tolerance, in every privilege
  mode, with neighbours, and on a full host. The figure is from the
  [experiments](docs/experiments.md).
- Linux on x86-64, with event tables for AMD Zen 3 to 5, Intel
  Skylake-SP through Granite Rapids and Sierra Forest, and Arm
  Neoverse. The AArch64 paths compile and have not yet run.

![Fields within tolerance per benchmark and privilege mode on the reference host](docs/experiments/plots/within.png)

## Documentation

Published as a book at <https://likewhatevs.github.io/thrashgen/>,
kept under `docs/`:

- [AGENTS.md](AGENTS.md): where work on the code starts, the goals,
  the rules and the method
- [Design](docs/design.md): the contract, every observable and knob,
  and what the hardware cannot do
- [Architecture](docs/architecture.md): the data flow, the modules,
  the run loop and the fitter's pass, as diagrams
- [Privileges](docs/privileges.md): what each privilege level buys
  and how measurement degrades without it
- [Testing](docs/testing.md): the three layers of tests and how to
  run them
- [Experiments](docs/experiments.md): the benchmark replays in every
  mode, their data and figures
- [Status](docs/status.md): what is verified, as of when, and what is
  open

## License

GNU General Public License, version 2 only; see `LICENSE`.
