//! CLI surface tests.

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_lists_every_subcommand() {
    let mut cmd = Command::cargo_bin("thrashgen").unwrap();
    cmd.arg("--help").assert().success().stdout(
        predicate::str::contains("caps")
            .and(predicate::str::contains("record"))
            .and(predicate::str::contains("aggregate"))
            .and(predicate::str::contains("run"))
            .and(predicate::str::contains("calibrate"))
            .and(predicate::str::contains("compare")),
    );
}

#[test]
fn unimplemented_subcommand_fails_cleanly() {
    let mut cmd = Command::cargo_bin("thrashgen").unwrap();
    cmd.arg("caps")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not implemented"));
}
