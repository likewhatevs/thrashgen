# thrashgen

build: `cargo build -q`. tests: `cargo test` (unit + fixture tests);
`cargo test --features hw-tests` needs a PMU, no privilege;
`--features root-tests` needs root: run it with the sudo env line in
docs/testing.md and CARGO_TARGET_DIR=target/root.
coverage: `cargo llvm-cov --summary-only`.
design: docs/design.md is the contract; update it in the same commit as
the code it describes. each observable's definition lives there once.
commits: one feature per commit, tests included.
