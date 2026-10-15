# thrashgen

build: `cargo build -q`. tests: `cargo test` (unit + fixture tests);
`sudo -E cargo test --features hw-tests` for hardware tests.
coverage: `cargo llvm-cov --summary-only`.
design: docs/design.md is the contract; update it in the same commit as
the code it describes. each observable's definition lives there once.
commits: one feature per commit, tests included.
