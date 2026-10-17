//! Calibration: knobs that reached a target set on one CPU model, kept
//! so a later run starts from them and so targets a run cannot measure
//! still get knobs that were verified somewhere.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::hash::Hasher;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::loadgen::Knobs;
use crate::profile::{CpuIdent, Targets};
use crate::report::{Report, Tier};

/// A saved fit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    /// CPU the knobs were fit on.
    pub cpu: Option<CpuIdent>,
    /// Event table used.
    pub table: Option<String>,
    /// Targets that were fit.
    pub targets: Targets,
    /// Knobs at the end of the fit.
    pub knobs: Knobs,
    /// Worker threads the knobs assume.
    pub threads: usize,
    /// Whether every measured target was within tolerance.
    pub converged: bool,
    /// Trust in each target's measurement when the knobs were fit.
    pub tiers: BTreeMap<String, Tier>,
}

impl Calibration {
    /// Build from a finished run.
    pub fn from_report(targets: &Targets, report: &Report) -> Calibration {
        Calibration {
            cpu: report.cpu.clone(),
            table: report.table.clone(),
            targets: targets.clone(),
            knobs: report.knobs.clone(),
            threads: report.threads,
            converged: report.converged,
            tiers: report
                .targets
                .iter()
                .map(|t| (t.key.clone(), t.tier))
                .collect(),
        }
    }

    /// Whether the knobs were fit on the same CPU model.
    pub fn matches(&self, cpu: Option<&CpuIdent>) -> bool {
        match (&self.cpu, cpu) {
            (Some(a), Some(b)) => {
                (a.vendor.as_str(), a.family, a.model) == (b.vendor.as_str(), b.family, b.model)
            }
            _ => false,
        }
    }

    /// JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("calibration serializes");
        s.push('\n');
        s
    }

    /// Read a calibration file.
    pub fn load(path: &Path) -> Result<Calibration> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parse calibration {}", path.display()))
    }

    /// Write a calibration file.
    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_json()).with_context(|| format!("write {}", path.display()))
    }
}

/// Directory of cached fits: `$XDG_CACHE_HOME/thrashgen`, else
/// `~/.cache/thrashgen`; none without a home.
pub fn cache_dir() -> Option<PathBuf> {
    cache_dir_from(
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// [`cache_dir`] from the two variables it reads.
pub fn cache_dir_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let base = match xdg {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(home.filter(|h| !h.is_empty())?).join(".cache"),
    };
    Some(base.join("thrashgen"))
}

/// File name of the cached fit for a CPU model and target set: a hash
/// of the vendor, family and model and of the targets as JSON, whose
/// keys are sorted. Threads are not part of it; the cached fit carries
/// the count it was made with.
pub fn cache_key(cpu: Option<&CpuIdent>, targets: &Targets) -> String {
    let mut h = fnv::FnvHasher::default();
    if let Some(c) = cpu {
        h.write(format!("{}/{}/{}", c.vendor, c.family, c.model).as_bytes());
    }
    h.write(
        serde_json::to_string(targets)
            .expect("targets serialize")
            .as_bytes(),
    );
    format!("{:016x}.json", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::TargetRow;

    #[test]
    fn cache_dir_follows_xdg_then_home() {
        let d =
            |x: Option<&str>, h: Option<&str>| cache_dir_from(x.map(OsStr::new), h.map(OsStr::new));
        assert_eq!(
            d(Some("/c"), Some("/h")),
            Some(PathBuf::from("/c/thrashgen"))
        );
        assert_eq!(
            d(Some(""), Some("/h")),
            Some(PathBuf::from("/h/.cache/thrashgen"))
        );
        assert_eq!(
            d(None, Some("/h")),
            Some(PathBuf::from("/h/.cache/thrashgen"))
        );
        assert_eq!(d(None, None), None);
        assert_eq!(d(None, Some("")), None);
    }

    #[test]
    fn cache_key_depends_on_cpu_model_and_targets_only() {
        let mut a = Targets::new();
        a.set("ipc", 1.0).unwrap();
        let mut b = Targets::new();
        b.set("ipc", 1.5).unwrap();
        let zen = CpuIdent {
            vendor: "amd".into(),
            family: 25,
            model: 24,
            name: "one name".into(),
        };
        let same_model = CpuIdent {
            name: "another name".into(),
            ..zen.clone()
        };
        let other = CpuIdent {
            model: 1,
            ..zen.clone()
        };
        let k = cache_key(Some(&zen), &a);
        assert_eq!(k.len(), 16 + 5);
        assert_eq!(k, cache_key(Some(&same_model), &a), "the name is not in it");
        assert_ne!(k, cache_key(Some(&other), &a));
        assert_ne!(k, cache_key(Some(&zen), &b));
        assert_ne!(k, cache_key(None, &a));
    }

    fn report(cpu: Option<CpuIdent>) -> Report {
        Report {
            cpu,
            table: Some("amdzen4".into()),
            threads: 3,
            pinned_cpus: None,
            knobs: Knobs {
                ops: 77,
                ..Knobs::default()
            },
            main_bytes: 1,
            thp_bytes: None,
            passes: 2,
            converged: true,
            targets: vec![TargetRow {
                key: "ipc".into(),
                target: 1.0,
                tier: Tier::Proxy,
                provenance: None,
                measured: Some(1.0),
                error: Some(0.0),
                within: true,
            }],
            memory: vec![],
            windows: vec![],
            notes: vec![],
        }
    }

    #[test]
    fn roundtrip_and_matching() {
        let amd = CpuIdent {
            vendor: "amd".into(),
            family: 25,
            model: 24,
            name: "A".into(),
        };
        let mut targets = Targets::new();
        targets.set("ipc", 1.0).unwrap();
        let c = Calibration::from_report(&targets, &report(Some(amd.clone())));
        assert_eq!(c.knobs.ops, 77);
        assert_eq!(c.threads, 3);
        assert_eq!(c.tiers["ipc"], Tier::Proxy);
        assert!(c.matches(Some(&amd)));
        assert!(c.matches(Some(&CpuIdent {
            name: "other name".into(),
            ..amd.clone()
        })));
        assert!(!c.matches(Some(&CpuIdent {
            model: 1,
            ..amd.clone()
        })));
        assert!(!c.matches(None));
        assert!(!Calibration::from_report(&targets, &report(None)).matches(Some(&amd)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cal.json");
        c.save(&path).unwrap();
        assert_eq!(Calibration::load(&path).unwrap(), c);
        assert!(Calibration::load(Path::new("/nonexistent")).is_err());
        std::fs::write(&path, "{}").unwrap();
        assert!(Calibration::load(&path).is_err());
    }
}
