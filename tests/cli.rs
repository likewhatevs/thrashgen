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
fn caps_prints_facts_and_rules() {
    let mut cmd = Command::cargo_bin("thrashgen").unwrap();
    cmd.arg("caps").assert().success().stdout(
        predicate::str::contains("perf_event_paranoid")
            .and(predicate::str::contains("own threads, user mode"))
            .and(predicate::str::contains("page cache residency")),
    );
}

#[test]
fn record_then_aggregate_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let rec = dir.path().join("rec.jsonl");
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "record",
            "--interval",
            "0.3",
            "--cpu-interval",
            "0.1",
            "--duration",
            "0.35",
            "-o",
        ])
        .arg(&rec)
        .args([
            "--",
            "sh",
            "-c",
            "i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("recorded"));
    let out = Command::cargo_bin("thrashgen")
        .unwrap()
        .arg("aggregate")
        .arg(&rec)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let profile =
        thrashgen::profile::Profile::from_json(std::str::from_utf8(&out).unwrap()).unwrap();
    assert_eq!(profile.count, 1);
    assert!(profile.avg["cpu_usage_cores"] > 0.0, "{:?}", profile.avg);
    assert!(profile.avg.contains_key("anon_bytes"));
    assert!(profile.provenance.contains_key("cpu_usage_cores"));
}

#[test]
fn record_requires_exactly_one_target() {
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args(["record", "--duration", "0.1"])
        .assert()
        .failure();
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args(["record", "--host", "--pid", "1", "--duration", "0.1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn unimplemented_subcommand_fails_cleanly() {
    let mut cmd = Command::cargo_bin("thrashgen").unwrap();
    cmd.arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not implemented"));
}

#[test]
fn aggregate_fixture_to_stdout_and_file() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/recording.jsonl"
    );
    let out = Command::cargo_bin("thrashgen")
        .unwrap()
        .args(["aggregate", fixture, "--pctiles", "50,90"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let profile =
        thrashgen::profile::Profile::from_json(std::str::from_utf8(&out).unwrap()).unwrap();
    assert_eq!(profile.count, 2);
    assert_eq!(profile.avg["ipc"], 2.0, "ratio of sums: 4e9 / 2e9");
    assert_eq!(profile.avg["anon_bytes"], 200.0);
    assert_eq!(profile.avg["cpu_limit_cores"], 2.0);
    assert!((profile.avg["cpu_usage_cv_1s"] - 0.5f64.sqrt()).abs() < 1e-12);
    assert_eq!(
        profile.pctiles["ipc"].keys().cloned().collect::<Vec<_>>(),
        ["p50", "p90"]
    );
    assert_eq!(profile.cpu.unwrap().vendor, "amd");
    assert_eq!(profile.provenance.len(), 2);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("profile.json");
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args(["aggregate", fixture, fixture, "-o"])
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    let twice =
        thrashgen::profile::Profile::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        twice.count, 4,
        "the same file twice is twice the containers"
    );
}

#[test]
fn aggregate_reports_bad_input() {
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args(["aggregate", "/nonexistent/recording.jsonl"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "open /nonexistent/recording.jsonl",
        ));

    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.jsonl");
    std::fs::write(
        &bad,
        "{\"kind\":\"cpu\",\"t\":1,\"scope\":\"x\",\"cores\":1}\n",
    )
    .unwrap();
    Command::cargo_bin("thrashgen")
        .unwrap()
        .arg("aggregate")
        .arg(&bad)
        .assert()
        .failure()
        .stderr(predicate::str::contains("line 1 is not a header"));

    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/recording.jsonl"
    );
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args(["aggregate", fixture, "--pctiles", "150"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("percentile 150"));
}
