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

Status: under construction. Subcommands are listed by `thrashgen --help`
and implemented in the order given in `docs/design.md`.
