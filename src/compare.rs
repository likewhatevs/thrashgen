//! Compare what was asked for with what was measured: a profile
//! column against a run report, a recording, or another profile.

use std::collections::BTreeMap;
use std::fmt;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::aggregate;
use crate::profile::{Pick, Profile, Role, Targets};
use crate::report::Report;
use crate::samples::Recording;

/// One field compared.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// Profile key.
    pub key: String,
    /// Value asked for.
    pub expected: f64,
    /// Value found, if present.
    pub actual: Option<f64>,
    /// `(actual - expected) / expected`.
    pub error: Option<f64>,
    /// Within tolerance; `None` when not measured.
    pub within: Option<bool>,
}

/// The comparison.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// Relative tolerance applied to every row.
    pub tolerance: f64,
    /// Target fields of the expected set, in field order.
    pub rows: Vec<Row>,
}

impl Comparison {
    /// Rows that were measured and within tolerance.
    pub fn within(&self) -> usize {
        self.rows.iter().filter(|r| r.within == Some(true)).count()
    }

    /// Rows that were measured.
    pub fn measured(&self) -> usize {
        self.rows.iter().filter(|r| r.actual.is_some()).count()
    }

    /// JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("comparison serializes");
        s.push('\n');
        s
    }
}

/// Compare the target fields of `expected` against `actual`: within
/// tolerance when the relative error is at most `tolerance` or the
/// absolute error is under the field's floor from
/// [`crate::fit::absolute_tolerance`].
pub fn compare(expected: &Targets, actual: &BTreeMap<String, f64>, tolerance: f64) -> Comparison {
    let rows = expected
        .iter()
        .filter(|(f, _)| f.role == Role::Target)
        .map(|(f, e)| {
            let a = actual.get(f.key).copied();
            let error = a.map(|a| {
                if e > 0.0 {
                    (a - e) / e
                } else if a == 0.0 {
                    0.0
                } else {
                    f64::INFINITY
                }
            });
            Row {
                key: f.key.into(),
                expected: e,
                actual: a,
                error,
                within: a.map(|a| {
                    (a - e).abs() <= crate::fit::absolute_tolerance(f.key)
                        || error.is_some_and(|err| err.abs() <= tolerance)
                }),
            }
        })
        .collect();
    Comparison { tolerance, rows }
}

/// A report's last-window measurements, targets and memory together.
pub fn actual_from_report(r: &Report) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for t in &r.targets {
        if let Some(v) = t.measured {
            out.insert(t.key.clone(), v);
        }
    }
    for m in &r.memory {
        if let Some(v) = m.measured {
            out.insert(m.key.clone(), v);
        }
    }
    out
}

/// What a file holds.
#[derive(Debug)]
pub enum Actual {
    /// A run report.
    Report(Box<Report>),
    /// A profile.
    Profile(Profile),
    /// A recording.
    Recording(Box<Recording>),
}

/// Load a report, profile or recording, telling them apart by content.
pub fn load_actual(path: &Path) -> Result<Actual> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let first = text.trim_start();
    if first.starts_with("{\"kind\":\"header\"") || first.starts_with("{\"kind\": \"header\"") {
        let rec = Recording::read(BufReader::new(text.as_bytes()))
            .with_context(|| format!("read recording {}", path.display()))?;
        return Ok(Actual::Recording(Box::new(rec)));
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if value.get("windows").is_some() && value.get("targets").is_some() {
        let r: Report = serde_json::from_value(value)
            .with_context(|| format!("parse report {}", path.display()))?;
        return Ok(Actual::Report(Box::new(r)));
    }
    if value.get("avg").is_some() {
        let p = Profile::from_json(&text)
            .with_context(|| format!("parse profile {}", path.display()))?;
        return Ok(Actual::Profile(p));
    }
    bail!(
        "{} is neither a report, a profile nor a recording",
        path.display()
    )
}

/// The flat values of an actual, picking a profile column where one applies.
pub fn actual_values(actual: &Actual, pick: &Pick) -> Result<BTreeMap<String, f64>> {
    Ok(match actual {
        Actual::Report(r) => actual_from_report(r),
        Actual::Profile(p) => p
            .pick(pick)?
            .iter()
            .map(|(f, v)| (f.key.to_string(), v))
            .collect(),
        Actual::Recording(rec) => {
            let profile =
                aggregate::aggregate(std::slice::from_ref(rec), &aggregate::Options::default())?;
            profile.avg
        }
    })
}

fn human(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{:.3}G", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.3}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.3}k", v / 1e3)
    } else {
        format!("{v:.4}")
    }
}

impl fmt::Display for Comparison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{:22} {:>12} {:>12} {:>8}  within {:.0}%",
            "target",
            "expected",
            "actual",
            "error",
            self.tolerance * 100.0
        )?;
        for r in &self.rows {
            writeln!(
                f,
                "{:22} {:>12} {:>12} {:>8}  {}",
                r.key,
                human(r.expected),
                r.actual.map_or_else(|| "-".into(), human),
                r.error
                    .map_or_else(|| "-".into(), |e| format!("{:+.1}%", e * 100.0)),
                match r.within {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "-",
                }
            )?;
        }
        writeln!(
            f,
            "{} of {} measured targets within tolerance, {} not measured",
            self.within(),
            self.measured(),
            self.rows.len() - self.measured()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> Targets {
        let mut t = Targets::new();
        t.set("ipc", 1.0).unwrap();
        t.set("l3_mpki", 4.0).unwrap();
        t.set("anon_bytes", 1000.0).unwrap();
        t.set("cpu_limit_cores", 2.0).unwrap();
        t
    }

    #[test]
    fn rows_errors_and_summary() {
        let actual = BTreeMap::from([("ipc".to_string(), 1.04), ("l3_mpki".to_string(), 5.0)]);
        let c = compare(&targets(), &actual, 0.05);
        assert_eq!(
            c.rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            ["ipc", "l3_mpki", "anon_bytes"],
            "hints are not compared"
        );
        assert_eq!(c.rows[0].within, Some(true));
        assert!((c.rows[0].error.unwrap() - 0.04).abs() < 1e-12);
        assert_eq!(c.rows[1].within, Some(false));
        assert_eq!(
            c.rows[2],
            Row {
                key: "anon_bytes".into(),
                expected: 1000.0,
                actual: None,
                error: None,
                within: None
            }
        );
        assert_eq!((c.within(), c.measured()), (1, 2));
        let text = c.to_string();
        assert!(text.contains("ipc                          1.0000       1.0400    +4.0%  yes"));
        assert!(text.contains("anon_bytes                   1.000k            -        -  -"));
        assert!(text.contains("1 of 2 measured targets within tolerance, 1 not measured"));
        let back: Comparison = serde_json::from_str(&c.to_json()).unwrap();
        assert_eq!(back, c);
        let mut zero = Targets::new();
        zero.set("itlb_pki", 0.0).unwrap();
        let c = compare(
            &zero,
            &BTreeMap::from([("itlb_pki".to_string(), 0.0)]),
            0.05,
        );
        assert_eq!(c.rows[0].error, Some(0.0));
        let c = compare(
            &zero,
            &BTreeMap::from([("itlb_pki".to_string(), 0.5)]),
            0.05,
        );
        assert_eq!(c.rows[0].error, Some(f64::INFINITY));
        assert_eq!(c.rows[0].within, Some(false));
        let c = compare(
            &zero,
            &BTreeMap::from([("itlb_pki".to_string(), 0.005)]),
            0.05,
        );
        assert_eq!(c.rows[0].within, Some(true), "under the absolute floor");
    }

    #[test]
    fn loads_each_kind_by_content() {
        let fixtures = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"));
        let profile = load_actual(&fixtures.join("profile.json")).unwrap();
        assert!(matches!(profile, Actual::Profile(_)));
        let v = actual_values(&profile, &Pick::Pctile("p80".into())).unwrap();
        assert_eq!(v["ipc"], 1.6);
        let rec = load_actual(&fixtures.join("recording.jsonl")).unwrap();
        assert!(matches!(rec, Actual::Recording(_)));
        let v = actual_values(&rec, &Pick::Avg).unwrap();
        assert_eq!(v["ipc"], 2.0);

        let dir = tempfile::tempdir().unwrap();
        let report = Report {
            cpu: None,
            table: None,
            threads: 1,
            pinned_cpus: None,
            knobs: crate::loadgen::Knobs::default(),
            main_bytes: 0,
            thp_bytes: None,
            passes: 0,
            converged: true,
            targets: vec![crate::report::TargetRow {
                key: "ipc".into(),
                target: 1.0,
                tier: crate::report::Tier::Verified,
                provenance: None,
                measured: Some(0.9),
                error: Some(-0.1),
                within: false,
            }],
            memory: vec![crate::report::MemoryRow {
                key: "anon_bytes".into(),
                target: Some(1000.0),
                measured: Some(990.0),
            }],
            windows: vec![],
            notes: vec![],
        };
        let path = dir.path().join("report.json");
        std::fs::write(&path, report.to_json()).unwrap();
        let loaded = load_actual(&path).unwrap();
        assert!(matches!(loaded, Actual::Report(_)));
        let v = actual_values(&loaded, &Pick::Avg).unwrap();
        assert_eq!((v["ipc"], v["anon_bytes"]), (0.9, 990.0));

        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{\"x\": 1}").unwrap();
        assert!(
            load_actual(&bad)
                .unwrap_err()
                .to_string()
                .contains("neither")
        );
        std::fs::write(&bad, "not json").unwrap();
        assert!(load_actual(&bad).is_err());
        assert!(load_actual(Path::new("/nonexistent")).is_err());
    }
}
