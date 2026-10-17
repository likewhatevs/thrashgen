# thrashgen

Instructions for anyone, human or agent, working in this repository.

build: `cargo build -q`. tests: `cargo test` (unit + fixture tests);
`cargo test --release --features hw-tests` needs a PMU, no privilege;
`--features root-tests` needs root: build unprivileged, run the test
binary with sudo as in docs/testing.md (`root_tests --test-threads=1`).
coverage: `cargo llvm-cov --summary-only`.
design: docs/design.md is the contract; update it in the same commit as
the code it describes. each observable's definition lives there once.
commits: one feature per commit, tests included; imperative subject,
body says why.
status: docs/status.md lists what is verified and what is open;
update it when an item closes.
measurement: through syscalls, sysfs and procfs via crates, never by
running other binaries; executing commands is for tests, the
experiment scripts, and `record -- cmd`.
