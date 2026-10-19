# thrashgen

Instructions for anyone, human or agent, working in this repository.
Read this first, then `docs/design.md` (the contract), then
`docs/architecture.md` (the map) and `docs/status.md` (what is
verified and what is open).

## What this is for

Record what a container does to its machine, as a small set of
observables, and generate synthetic load that does the same thing, so
a fleet's workloads can be stood in for by many instances of one tool.
The observables are fixed by the profile specification: busy cores and
their per-second CV, IPC, L3 misses per thousand instructions, DRAM
bandwidth, dTLB and iTLB events per thousand instructions, and the
split of memory into anonymous, page cache and shared. The tool never
needs an observable outside that set; a knob is a means to one of
them, never a new input.

Directional goals, in order:

1. **Fidelity.** A replay recorded the way the original was recorded
   lands every measured field within tolerance (5 percent, or an
   absolute floor per field). Where hardware makes that impossible,
   the design says so in section 5.1 and the report says so in its
   notes; a residual the tool cannot explain is a bug.
2. **Speed.** The fit converges in about a second on a cold start
   (0.4 to 0.8 s on the reference host for the profiles that converge;
   stress-ng reaches what it can by 1.0 s), so representative load is
   up by the fifth second, and a repeat run of the same profile on the
   same CPU model starts converged from the cache.
3. **Noisy hosts.** Several instances share a host, each fed its own
   container's profile and measuring only its own scope. A converged
   fit follows a real change in its surroundings and ignores a
   transient one: it moves again only after a second out of tolerance.
4. **Honesty about privilege.** Every measurement that needs a
   privilege the process lacks degrades to the next best source and is
   labelled as such. Nothing fails for lack of a counter, and nothing
   pretends to a precision it does not have.

## Non-negotiables

- `docs/design.md` is the contract: each observable's definition and
  each knob's effect lives there once. A change in what an observable
  means or how a knob moves it lands in the design in the same commit
  as the code.
- Measurement goes through syscalls, sysfs and procfs via crates,
  never by running other binaries. Executing commands is for tests,
  the experiment scripts and `record -- cmd`.
- The fitter's rules are pure functions of the last pooled window and
  the fitter's own state, so they are driven against a simulated
  machine in tests; a rule that needs a clock or a thread is in the
  wrong module.
- The generator must do the work the hardware is meant to see. Any
  dependency the compiler could fold, a `& 0` or a shift to zero, goes
  through `std::hint::black_box` on both operands; an address on the
  hot path comes from registers, never from a value that waits on
  memory. Verify such changes by measurement, not by reading the
  assembly.
- Within tolerance, a rule moves nothing. Every rule nudges, and a
  nudge from inside the band chases noise.
- No attribution lines, session links or local paths in commits,
  docs or published outputs. Published experiment data carries only
  the host's CPU model.

## Architecture in brief

Three artifacts travel between the commands: `record` writes a
recording (JSON lines, header plus samples with provenance),
`aggregate` folds recordings into a profile (averages, percentiles,
provenance), `run` fits a load to a profile and writes a report
(targets with their tiers, knobs, every window), `compare` puts two of
these side by side. Modules form a DAG: formats at the bottom
(`profile`, `samples`, `report`, `calibration`), measurement in the
middle (`caps` → `plan` → `perf`/`resctrl`/`measure` → `sampler`),
the generator (`footprint` → `loadgen`) and the fitter (`fit`) beside
it, and the commands on top (`record`, `aggregate`, `run`, `compare`,
`cli`). `docs/architecture.md` has the diagrams and the per-module
table; keep them current when a module's role changes.

The fit loop: 100 ms windows; consecutive windows at the same knobs
are pooled by instructions; busy cores and burstiness are judged from
the third one-second sample against what the generator intended for
each second; a pass is a window that moved a knob; thirty-six passes
at most for a fit that has not converged; a converged fit holds and
re-fits only after a second out. The rule order within a pass is in
`docs/architecture.md` section 4 and in the fitter's own comments.

## How to work here

- build: `cargo build -q`; the generator needs `--release` to be
  measured (debug spends some fifteen times the instructions per
  access).
- tests: `cargo test` (unit, fixture, CLI and plot tests); `cargo test
  --release --features hw-tests` needs a PMU, no privilege;
  `--features root-tests` needs root: build unprivileged, run the lib
  test binary with sudo as in `docs/testing.md`. Never pass filters to
  skip tests; load-generating tests are serialized with `serial_test`.
- lints: `cargo fmt --check`, `cargo clippy --all-targets --
  -D warnings`, with and without the features. Clippy's pedantic group
  is on in `src/lib.rs`; the lints allowed there each carry a reason,
  and a new allow needs one too. Coverage: `cargo llvm-cov
  --summary-only`. CI runs the two lints and nothing else, by choice;
  tests and experiments belong on a machine with counters.
- commits: one feature per commit, tests and design text included;
  imperative subject, body says why, not what.
- status: `docs/status.md` lists what is verified and what is open;
  update it when an item closes or a new one is found, and say which
  are physics (hardware floors, kernel limits) and which are work.

## Changing the fitter or the generator

The method that has worked, and the mistakes it replaced:

1. **Measure before theorizing.** Run the generator at fixed knobs
   (`run profile.json --knobs knobs.json`) and read the report's
   windows or `perf stat` the process with raw events
   (`cpu/event=0x45,umask=0x0f/u`; event aliases are often missing).
   Sweep one knob at a time. Every surprising result in this project's
   history, the serialized misses, the dTLB floor, the sparse-set
   refills, the quiet-second miss inflation, the run length that
   stops moving MPKI at one op, the page-cache read that moves no
   bytes from a file the L3 holds, was found this way and not by
   reasoning about the code.
2. **Then encode it as a rule with a model.** Steps are proportional
   to the error through a model of the lever (misses per access over
   the lines a page holds, the instruction slope against ops), damped,
   capped per pass, and quiet within tolerance. A fixed step or a seed
   value is a limit cycle waiting to happen.
3. **Give every rule a plant test.** `fit.rs` has a simulated machine
   (`plant`) and direct `step` tests for single rules; a new rule gets
   both. The plant is a caricature, so also time cold fits on the
   recorded profiles under `docs/experiments/user/*/orig.json` with
   `--duration 8 --no-cache` before and after, and look at the pass
   trajectories, not only the final table.
4. **Mind the budget.** The user's bar is a fit in about three
   seconds. A rule that needs more passes than that on the cold
   profiles needs a better model, not more passes.
5. **Separate physics from fit.** If a target cannot be reached at
   any knobs, say so in the design's section 5.1 and in the status,
   with the measurement that shows it, and make the fitter stop
   trying (the closest pass, a frozen lever) rather than cycle.

## Experiments

`experiments/all.sh` with `MODE=user|kernel|resctrl|noisy|perfmon`
records each benchmark, fits, holds the fitted knobs without counters,
records the hold and compares; `MODE=crowd` replays four copies of
three profiles at once and `MODE=disturbed` fits each live while a
stress-ng starts and stops beside it, both from a finished user set;
`experiments/publish.sh` copies the small outputs into
`docs/experiments/<mode>/`, `experiments/plot.py` draws the figures.
`experiments/levers.sh` measures what each lever of the generator
costs per unit at fixed knobs; run it before and after a change to
the access path or a compiler update and compare with the figures in
`docs/status.md`.
A full chain of the five privilege modes takes about two hours on the
reference host, the two neighbour modes ten minutes, and none of it
may overlap with builds, tests or other load, since the recordings
are the evidence. Never edit a script while the chain is executing
it, and never rebuild while it runs: each mode builds from the
working tree. `docs/experiments.md` is rewritten from the published
results, not edited by hand to match expectations.

## Style

Comments say why, at the density of the surrounding code, and never
assert behaviour that was not measured. Prose in docs and commit
messages is plain: what was observed, what was changed, what it bought.
Numbers come with the host and the date they were measured on.
