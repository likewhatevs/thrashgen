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
            .and(predicate::str::contains("page cache residency"))
            .and(predicate::str::contains("probe: per-CPU")),
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
#[serial_test::serial]
fn run_fits_busy_cores_and_memory_in_its_own_process() {
    let dir = tempfile::tempdir().unwrap();
    let report_path = dir.path().join("report.json");
    let assert = Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "run",
            "--set",
            "cpu_usage_cores=0.5",
            "--set",
            "anon_bytes=100663296",
            "--set",
            "pagecache_bytes=8388608",
            "--set",
            "shmem_bytes=4194304",
        ])
        .args([
            "--duration",
            "2.6",
            "--window",
            "0.8",
            "--pin",
            "no",
            "--threads",
            "1",
            "--sparse-pages",
            "64",
            "--code-pages",
            "64",
            "--no-huge",
        ])
        .arg("--scratch-dir")
        .arg(dir.path())
        .arg("--report")
        .arg(&report_path)
        .env("XDG_CACHE_HOME", dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("cpu_usage_cores"))
        .stderr(predicate::str::contains("window"));
    let _ = assert;
    let report: thrashgen::report::Report =
        serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();
    assert!(report.windows.len() >= 2, "{report:#?}");
    assert_eq!(report.threads, 1);
    assert_eq!(report.pinned_cpus, None);
    let cores = report
        .targets
        .iter()
        .find(|r| r.key == "cpu_usage_cores")
        .unwrap();
    assert_eq!(cores.tier, thrashgen::report::Tier::Verified);
    let measured = cores.measured.unwrap();
    assert!((0.3..=0.75).contains(&measured), "{measured}");
    let anon = report
        .memory
        .iter()
        .find(|m| m.key == "anon_bytes")
        .unwrap()
        .measured
        .unwrap();
    assert!(
        (anon - 100_663_296.0).abs() < 16.0 * f64::from(1 << 20),
        "{anon}"
    );
    let cache = report
        .memory
        .iter()
        .find(|m| m.key == "pagecache_bytes")
        .unwrap()
        .measured
        .unwrap();
    assert!(cache > 7.0 * f64::from(1 << 20), "{cache}");
    assert!(report.converged || report.passes >= 1, "{report:#?}");
}

#[test]
#[serial_test::serial]
fn run_starts_from_its_cached_fit() {
    let dir = tempfile::tempdir().unwrap();
    let run = |extra: &[&str]| {
        let mut cmd = Command::cargo_bin("thrashgen").unwrap();
        // Busy cores are judged from the third second; six leave room
        // for a loaded host to settle twice.
        cmd.args(["run", "--set", "cpu_usage_cores=0.5", "--set", "ipc=1.2"])
            .args([
                "--duration",
                "6",
                "--window",
                "0.5",
                "--pin",
                "no",
                "--threads",
                "1",
            ])
            .args(["--sparse-pages", "64", "--code-pages", "64", "--no-huge"])
            .args(extra)
            .arg("--scratch-dir")
            .arg(dir.path())
            .env("XDG_CACHE_HOME", dir.path());
        cmd.assert().success()
    };
    let first = run(&[]);
    let cache = dir.path().join("thrashgen");
    let entries: Vec<_> = std::fs::read_dir(&cache)
        .unwrap_or_else(|e| panic!("no cache written at {}: {e}", cache.display()))
        .flatten()
        .collect();
    assert_eq!(entries.len(), 1, "one cached fit: {entries:?}");
    let cal: thrashgen::calibration::Calibration =
        serde_json::from_str(&std::fs::read_to_string(entries[0].path()).unwrap()).unwrap();
    assert_eq!(cal.threads, 1);
    first.stderr(predicate::str::contains("fit kept at"));
    run(&[]).stderr(predicate::str::contains("starting from the fit cached at"));
    run(&["--no-cache"]).stderr(predicate::str::contains("cached").not());
}

#[test]
#[serial_test::serial]
fn run_takes_a_page_cache_target_below_one_page() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "run",
            "--set",
            "cpu_usage_cores=0.3",
            "--set",
            "pagecache_bytes=100",
        ])
        .args([
            "--duration",
            "0.6",
            "--window",
            "0.2",
            "--pin",
            "no",
            "--threads",
            "1",
        ])
        .args([
            "--sparse-pages",
            "64",
            "--code-pages",
            "64",
            "--no-huge",
            "--no-cache",
        ])
        .arg("--scratch-dir")
        .arg(dir.path())
        .env("XDG_CACHE_HOME", dir.path())
        .assert()
        .success();
}

#[test]
fn run_rejects_bad_arguments() {
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args(["run", "--duration", "0.1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no targets"));
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "run",
            "--set",
            "ipc=1",
            "--pin",
            "maybe",
            "--duration",
            "0.1",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--pin must be"));
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "run",
            "--set",
            "ipc=1",
            "--main-bytes",
            "1X",
            "--duration",
            "0.1",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("size suffix"));
}

#[test]
fn compare_profile_against_recording_and_itself() {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "compare",
            &format!("{fixtures}/profile.json"),
            &format!("{fixtures}/recording.jsonl"),
            "--pick",
            "p80",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ipc").and(predicate::str::contains(
                "measured targets within tolerance",
            )),
        );
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("cmp.json");
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "compare",
            &format!("{fixtures}/profile.json"),
            &format!("{fixtures}/profile.json"),
            "--json",
        ])
        .arg(&json)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "11 of 11 measured targets within tolerance",
        ));
    let c: thrashgen::compare::Comparison =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(c.within(), 11);
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "compare",
            &format!("{fixtures}/profile.json"),
            "/nonexistent",
        ])
        .assert()
        .failure();
}

#[test]
#[serial_test::serial]
fn calibrate_then_run_from_the_calibration() {
    let dir = tempfile::tempdir().unwrap();
    let cal = dir.path().join("cal.json");
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "calibrate",
            "--set",
            "cpu_usage_cores=0.3",
            "--duration",
            "3",
            "--window",
            "0.6",
            "--pin",
            "no",
            "--threads",
            "1",
        ])
        .args([
            "--main-bytes",
            "4M",
            "--sparse-pages",
            "16",
            "--code-pages",
            "16",
            "--no-huge",
            "-o",
        ])
        .arg(&cal)
        .arg("--scratch-dir")
        .arg(dir.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("calibration written"));
    let loaded = thrashgen::calibration::Calibration::load(&cal).unwrap();
    assert_eq!(loaded.threads, 1);
    assert_eq!(loaded.targets.get("cpu_usage_cores"), Some(0.3));
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "run",
            "--set",
            "cpu_usage_cores=0.3",
            "--duration",
            "1.2",
            "--window",
            "0.5",
            "--pin",
            "no",
            "--calibration",
        ])
        .arg(&cal)
        .args([
            "--main-bytes",
            "4M",
            "--sparse-pages",
            "16",
            "--code-pages",
            "16",
            "--no-huge",
            "--scratch-dir",
        ])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("1 threads"));
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "calibrate",
            "--set",
            "ipc=1",
            "--knobs",
            "/nonexistent",
            "-o",
        ])
        .arg(&cal)
        .assert()
        .failure();
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

#[cfg(feature = "hw-tests")]
#[test]
#[serial_test::serial]
fn run_fits_counter_targets_on_real_hardware() {
    // An unoptimized access costs some fifteen times the instructions of
    // an optimized one, and no knob reaches eight misses per thousand of
    // them; the fit is a property of the release build, as the
    // experiments run it.
    if cfg!(debug_assertions) {
        panic!(
            "the hardware fit test needs the optimized binary: cargo test --release --features hw-tests"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let report_path = dir.path().join("report.json");
    Command::cargo_bin("thrashgen")
        .unwrap()
        .args([
            "run",
            "--set",
            "cpu_usage_cores=2",
            "--set",
            "ipc=0.8",
            "--set",
            "l3_mpki=8",
            "--set",
            "dtlb_pki=1",
            "--set",
            "itlb_pki=0.05",
        ])
        .args([
            "--duration",
            "40",
            "--window",
            "5",
            "--pin",
            "no",
            "--main-bytes",
            "1G",
            "--report",
        ])
        .arg(&report_path)
        .arg("--scratch-dir")
        .arg(dir.path())
        .assert()
        .success();
    let report: thrashgen::report::Report =
        serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();
    let row = |key: &str| {
        report
            .targets
            .iter()
            .find(|r| r.key == key)
            .unwrap()
            .clone()
    };
    assert_eq!(row("ipc").tier, thrashgen::report::Tier::Verified);
    assert!(matches!(
        row("l3_mpki").tier,
        thrashgen::report::Tier::Verified | thrashgen::report::Tier::Proxy
    ));
    assert_eq!(row("dtlb_pki").tier, thrashgen::report::Tier::Verified);
    assert!(report.windows.len() >= 6, "{}", report.windows.len());
    let first = report.windows.first().unwrap();
    let last = report.windows.last().unwrap();
    let err = |w: &thrashgen::report::WindowRow, key: &str| {
        w.values
            .iter()
            .find(|v| v.key == key)
            .unwrap()
            .error
            .unwrap()
            .abs()
    };
    for key in ["l3_mpki", "dtlb_pki", "itlb_pki"] {
        assert!(
            err(last, key) <= err(first, key).max(0.1),
            "{key}: first {} last {}",
            err(first, key),
            err(last, key)
        );
    }
    assert!(err(last, "l3_mpki") < 0.3, "{}", err(last, "l3_mpki"));
}
