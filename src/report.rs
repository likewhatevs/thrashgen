//! The run report: what was asked, what was measured at which tier,
//! and the knobs that got there. Written as JSON and shown as a table.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::loadgen::Knobs;
use crate::profile::{CpuIdent, Method, Provenance};

/// How much a measured target can be trusted: `docs/design.md`
/// section 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Measured with the recorder's definition.
    Verified,
    /// Measured with a counter known to track the definition.
    Proxy,
    /// Computed from what the generator emitted.
    Modelled,
    /// Not measured; run open-loop.
    Unverified,
}

impl Tier {
    /// Tier of a field by how its source was obtained.
    pub fn of(provenance: Option<&Provenance>) -> Tier {
        match provenance.map(|p| p.method) {
            None => Tier::Unverified,
            Some(Method::Proxy) => Tier::Proxy,
            Some(Method::Model) => Tier::Modelled,
            Some(_) => Tier::Verified,
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tier::Verified => "verified",
            Tier::Proxy => "proxy",
            Tier::Modelled => "modelled",
            Tier::Unverified => "unverified",
        })
    }
}

/// One target's outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetRow {
    /// Profile key.
    pub key: String,
    /// Requested value.
    pub target: f64,
    /// Trust in the measurement.
    pub tier: Tier,
    /// Where the measurement came from, if measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// Value in the last window.
    pub measured: Option<f64>,
    /// Relative error in the last window.
    pub error: Option<f64>,
    /// Within tolerance in the last window.
    pub within: bool,
}

/// One target in one window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowValue {
    /// Profile key.
    pub key: String,
    /// Measured value.
    pub measured: Option<f64>,
    /// Relative error.
    pub error: Option<f64>,
}

/// One measurement window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowRow {
    /// Seconds since the run started, at the window's end.
    pub end_s: f64,
    /// Window length.
    pub seconds: f64,
    /// Fitter pass the window was measured under.
    pub pass: u32,
    /// Knobs in force during the window.
    pub knobs: Knobs,
    /// Every target's value.
    pub values: Vec<WindowValue>,
}

/// A memory component: requested and finally measured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryRow {
    /// Profile key.
    pub key: String,
    /// Requested bytes.
    pub target: Option<f64>,
    /// Measured bytes in the last window.
    pub measured: Option<f64>,
}

/// The whole report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// CPU the run was on.
    pub cpu: Option<CpuIdent>,
    /// Event table used, if any.
    pub table: Option<String>,
    /// Worker threads.
    pub threads: usize,
    /// CPUs the workers were pinned to, one per worker, if pinned.
    pub pinned_cpus: Option<Vec<u32>>,
    /// Final knobs.
    pub knobs: Knobs,
    /// Main buffer size.
    pub main_bytes: u64,
    /// Bytes of the main buffer backed by transparent hugepages.
    pub thp_bytes: Option<u64>,
    /// Fitter passes taken.
    pub passes: u32,
    /// Whether every measured target was within tolerance at the end.
    pub converged: bool,
    /// Targets, in field order.
    pub targets: Vec<TargetRow>,
    /// Memory components.
    pub memory: Vec<MemoryRow>,
    /// Windows, oldest first.
    pub windows: Vec<WindowRow>,
    /// Fallbacks and warnings.
    pub notes: Vec<String>,
}

fn human(v: f64) -> String {
    let a = v.abs();
    if a >= 1e12 {
        format!("{:.3}T", v / 1e12)
    } else if a >= 1e9 {
        format!("{:.3}G", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.3}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.3}k", v / 1e3)
    } else {
        format!("{v:.4}")
    }
}

impl Report {
    /// JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("report serializes");
        s.push('\n');
        s
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(c) = &self.cpu {
            writeln!(
                f,
                "cpu       {} (family {} model {}), table {}",
                c.name,
                c.family,
                c.model,
                self.table.as_deref().unwrap_or("none")
            )?;
        }
        writeln!(
            f,
            "workers   {} threads{}",
            self.threads,
            match &self.pinned_cpus {
                Some(c) => format!(
                    ", pinned to {}",
                    c.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
                ),
                None => String::new(),
            }
        )?;
        writeln!(
            f,
            "memory    main buffer {}B{}",
            human(self.main_bytes as f64),
            self.thp_bytes
                .map(|t| format!(", {}B hugepage-backed", human(t as f64)))
                .unwrap_or_default()
        )?;
        writeln!(
            f,
            "fit       {} passes, {}",
            self.passes,
            if self.converged {
                "converged"
            } else {
                "not converged"
            }
        )?;
        writeln!(f)?;
        writeln!(
            f,
            "{:22} {:10} {:>12} {:>12} {:>8}",
            "target", "tier", "requested", "measured", "error"
        )?;
        for t in &self.targets {
            writeln!(
                f,
                "{:22} {:10} {:>12} {:>12} {:>8}",
                t.key,
                t.tier.to_string(),
                human(t.target),
                t.measured.map(human).unwrap_or_else(|| "-".into()),
                t.error
                    .map(|e| format!("{:+.1}%", e * 100.0))
                    .unwrap_or_else(|| "-".into())
            )?;
        }
        if !self.memory.is_empty() {
            writeln!(f)?;
            writeln!(f, "{:22} {:>12} {:>12}", "memory", "requested", "measured")?;
            for m in &self.memory {
                writeln!(
                    f,
                    "{:22} {:>12} {:>12}",
                    m.key,
                    m.target.map(human).unwrap_or_else(|| "-".into()),
                    m.measured.map(human).unwrap_or_else(|| "-".into())
                )?;
            }
        }
        writeln!(f)?;
        writeln!(
            f,
            "knobs     duty {:.3} cv {:.3} ops {} chains {:.2} lines {:.2} seq {:.2} dep {:.2} wset {:.3} wfrac {:.2} ntlines {:.3} tlbvisits {:.4} sparse_pages {} codevisits {:.4} code_pages {}",
            self.knobs.duty,
            self.knobs.cv,
            self.knobs.ops,
            self.knobs.chains,
            self.knobs.lines,
            self.knobs.seq,
            self.knobs.dep,
            self.knobs.wset,
            self.knobs.wfrac,
            self.knobs.ntlines,
            self.knobs.tlbvisits,
            self.knobs.sparse_pages,
            self.knobs.codevisits,
            self.knobs.code_pages
        )?;
        for n in &self.notes {
            writeln!(f, "note      {n}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Mode, Scope};

    #[test]
    fn tiers_follow_method() {
        let p = |method| Provenance {
            scope: Scope::Process,
            mode: Mode::User,
            method,
        };
        assert_eq!(Tier::of(None), Tier::Unverified);
        assert_eq!(Tier::of(Some(&p(Method::Perf))), Tier::Verified);
        assert_eq!(Tier::of(Some(&p(Method::Rusage))), Tier::Verified);
        assert_eq!(Tier::of(Some(&p(Method::Proxy))), Tier::Proxy);
        assert_eq!(Tier::of(Some(&p(Method::Model))), Tier::Modelled);
        assert_eq!(Tier::Modelled.to_string(), "modelled");
        assert_eq!(
            serde_json::to_string(&Tier::Unverified).unwrap(),
            "\"unverified\""
        );
    }

    #[test]
    fn report_roundtrips_and_prints() {
        let r = Report {
            cpu: Some(CpuIdent {
                vendor: "amd".into(),
                family: 25,
                model: 24,
                name: "X".into(),
            }),
            table: Some("amdzen4".into()),
            threads: 2,
            pinned_cpus: Some(vec![0, 1]),
            knobs: Knobs::default(),
            main_bytes: 1 << 30,
            thp_bytes: Some(1 << 29),
            passes: 3,
            converged: true,
            targets: vec![TargetRow {
                key: "ipc".into(),
                target: 1.2,
                tier: Tier::Verified,
                provenance: None,
                measured: Some(1.25),
                error: Some(0.0417),
                within: true,
            }],
            memory: vec![MemoryRow {
                key: "anon_bytes".into(),
                target: Some(4e9),
                measured: Some(4.01e9),
            }],
            windows: vec![WindowRow {
                end_s: 30.0,
                seconds: 30.0,
                pass: 1,
                knobs: Knobs::default(),
                values: vec![WindowValue {
                    key: "ipc".into(),
                    measured: Some(1.25),
                    error: Some(0.0417),
                }],
            }],
            notes: vec!["hello".into()],
        };
        let back: Report = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(back, r);
        let text = r.to_string();
        assert!(text.contains("cpu       X (family 25 model 24), table amdzen4"));
        assert!(text.contains("pinned to 0,1"));
        assert!(text.contains("1.074GB, 536.871MB hugepage-backed"));
        assert!(
            text.contains("ipc                    verified         1.2000       1.2500    +4.2%")
        );
        assert!(text.contains("anon_bytes                   4.000G       4.010G"));
        assert!(text.contains("note      hello"));
        assert!(text.contains("3 passes, converged"));
        assert_eq!(human(0.5), "0.5000");
        assert_eq!(human(1500.0), "1.500k");
        assert_eq!(human(2.5e12), "2.500T");
    }
}
