//! The profile: which observables exist, how each aggregates, where it
//! was measured, and the JSON file `aggregate` writes and `run` reads.
//!
//! `docs/design.md` section 2 defines every field; this module is the
//! executable form of that table.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// What a field is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Shapes the generator (thread count, limits) but is not a target.
    Hint,
    /// The fitter drives the measurement towards it.
    Target,
    /// Reported next to targets so a profile can be sanity-checked.
    Consistency,
    /// Describes where other fields came from.
    Provenance,
}

/// How samples of a field combine into a profile value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Agg {
    /// Mean of samples, then mean over scopes with equal weight.
    Mean,
    /// Sum of `num` over sum of `den`, times `scale`; never a mean of
    /// per-sample ratios.
    RatioOfSums {
        /// Key of the numerator rate.
        num: &'static str,
        /// Key of the denominator rate.
        den: &'static str,
        /// Multiplier, 1000 for per-kilo-instruction rates.
        scale: f64,
    },
    /// Pooled coefficient of variation of 1 Hz CPU samples over windows.
    PooledCv,
    /// Taken from the scope's limits, not aggregated.
    Static,
}

/// One row of the observable table.
#[derive(Clone, Copy, Debug)]
pub struct FieldSpec {
    /// Key used in profiles, samples and `--set`.
    pub key: &'static str,
    /// Unit, for reports.
    pub unit: &'static str,
    /// Role in fitting.
    pub role: Role,
    /// Aggregation rule.
    pub agg: Agg,
}

/// Every field a profile may contain, in report order.
pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "cpu_limit_cores",
        unit: "cores",
        role: Role::Hint,
        agg: Agg::Static,
    },
    FieldSpec {
        key: "memory_limit_bytes",
        unit: "bytes",
        role: Role::Hint,
        agg: Agg::Static,
    },
    FieldSpec {
        key: "cpu_usage_cores",
        unit: "cores",
        role: Role::Target,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "cpu_usage_cv_1s",
        unit: "",
        role: Role::Target,
        agg: Agg::PooledCv,
    },
    FieldSpec {
        key: "instructions_per_sec",
        unit: "1/s",
        role: Role::Consistency,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "cycles_per_sec",
        unit: "1/s",
        role: Role::Consistency,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "ipc",
        unit: "",
        role: Role::Target,
        agg: Agg::RatioOfSums {
            num: "instructions_per_sec",
            den: "cycles_per_sec",
            scale: 1.0,
        },
    },
    FieldSpec {
        key: "l3_misses_per_sec",
        unit: "1/s",
        role: Role::Consistency,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "l3_mpki",
        unit: "per 1000 instr",
        role: Role::Target,
        agg: Agg::RatioOfSums {
            num: "l3_misses_per_sec",
            den: "instructions_per_sec",
            scale: 1000.0,
        },
    },
    FieldSpec {
        key: "dram_bytes_per_sec",
        unit: "bytes/s",
        role: Role::Target,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "dtlb_misses_per_sec",
        unit: "1/s",
        role: Role::Consistency,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "dtlb_pki",
        unit: "per 1000 instr",
        role: Role::Target,
        agg: Agg::RatioOfSums {
            num: "dtlb_misses_per_sec",
            den: "instructions_per_sec",
            scale: 1000.0,
        },
    },
    FieldSpec {
        key: "itlb_misses_per_sec",
        unit: "1/s",
        role: Role::Consistency,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "itlb_pki",
        unit: "per 1000 instr",
        role: Role::Target,
        agg: Agg::RatioOfSums {
            num: "itlb_misses_per_sec",
            den: "instructions_per_sec",
            scale: 1000.0,
        },
    },
    FieldSpec {
        key: "memory_current_bytes",
        unit: "bytes",
        role: Role::Target,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "anon_bytes",
        unit: "bytes",
        role: Role::Target,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "pagecache_bytes",
        unit: "bytes",
        role: Role::Target,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "shmem_bytes",
        unit: "bytes",
        role: Role::Target,
        agg: Agg::Mean,
    },
    FieldSpec {
        key: "tlb_host_ipc",
        unit: "",
        role: Role::Provenance,
        agg: Agg::Static,
    },
    FieldSpec {
        key: "tlb_host_minutes",
        unit: "minutes",
        role: Role::Provenance,
        agg: Agg::Static,
    },
];

/// Look a field up by key.
pub fn field(key: &str) -> Option<&'static FieldSpec> {
    FIELDS.iter().find(|f| f.key == key)
}

/// Keys of every field with the given role, in table order.
pub fn keys_with_role(role: Role) -> impl Iterator<Item = &'static str> {
    FIELDS.iter().filter(move |f| f.role == role).map(|f| f.key)
}

/// Where a value was measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// A cgroup v2 directory.
    Cgroup,
    /// A task and its threads.
    Process,
    /// The whole host.
    Host,
}

/// Which CPU modes a counter covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// User mode only.
    User,
    /// User and kernel mode.
    All,
    /// Not a counter.
    NotApplicable,
}

/// How a value was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// `perf_event_open` with the vendor definition.
    Perf,
    /// cgroup v2 files.
    Cgroup,
    /// `/proc` files.
    Proc,
    /// `getrusage`.
    Rusage,
    /// `cachestat(2)`.
    Cachestat,
    /// resctrl monitoring files.
    Resctrl,
    /// A different counter known to track the definition.
    Proxy,
    /// Computed from what the generator emitted.
    Model,
}

/// Provenance of one profile field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Measurement scope.
    pub scope: Scope,
    /// Counting mode.
    pub mode: Mode,
    /// Source.
    pub method: Method,
}

/// The CPU a profile or calibration applies to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuIdent {
    /// `amd`, `intel` or `arm`.
    pub vendor: String,
    /// Display family (x86) or implementer part (ARM).
    pub family: u32,
    /// Display model (x86) or part number (ARM).
    pub model: u32,
    /// Marketing name from `/proc/cpuinfo`, for reports.
    pub name: String,
}

/// A profile as written by `aggregate` and read by `run`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Number of scopes the profile was built from.
    pub count: u64,
    /// CPU the recordings came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuIdent>,
    /// Per-field averages.
    pub avg: BTreeMap<String, f64>,
    /// Per-field percentiles, keyed like `p50`.
    #[serde(default)]
    pub pctiles: BTreeMap<String, BTreeMap<String, f64>>,
    /// Per-field provenance.
    #[serde(default)]
    pub provenance: BTreeMap<String, Provenance>,
}

/// Which column of a profile to replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// The `avg` column.
    Avg,
    /// One percentile, by label such as `p80`.
    Pctile(String),
}

impl std::str::FromStr for Pick {
    type Err = ProfileError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "avg" {
            return Ok(Pick::Avg);
        }
        if is_pctile_label(s) {
            return Ok(Pick::Pctile(s.to_string()));
        }
        Err(ProfileError::BadPick(s.to_string()))
    }
}

impl fmt::Display for Pick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pick::Avg => f.write_str("avg"),
            Pick::Pctile(p) => f.write_str(p),
        }
    }
}

/// `p` followed by a number in 0..=100, such as `p50` or `p99.9`.
pub fn is_pctile_label(s: &str) -> bool {
    s.strip_prefix('p')
        .and_then(|n| n.parse::<f64>().ok())
        .is_some_and(|v| (0.0..=100.0).contains(&v))
}

/// Errors from reading or selecting from a profile.
#[derive(Debug, Error, PartialEq)]
pub enum ProfileError {
    /// A key that is not in [`FIELDS`].
    #[error("unknown field `{0}`; known fields: {known}", known = known_keys())]
    UnknownField(String),
    /// A value that is negative, NaN or infinite.
    #[error("field `{key}` has invalid value {value}")]
    BadValue {
        /// Field key.
        key: String,
        /// Offending value.
        value: f64,
    },
    /// A percentile label that is not `pNN`.
    #[error("bad percentile label `{0}`; expected `avg` or `p<0..100>`")]
    BadPick(String),
    /// The requested column is absent for every field.
    #[error("profile has no `{0}` column")]
    MissingColumn(String),
    /// A `--set key=value` that does not parse.
    #[error("bad override `{0}`; expected key=value")]
    BadOverride(String),
}

fn known_keys() -> String {
    FIELDS.iter().map(|f| f.key).collect::<Vec<_>>().join(", ")
}

fn check_value(key: &str, value: f64) -> Result<(), ProfileError> {
    if field(key).is_none() {
        return Err(ProfileError::UnknownField(key.to_string()));
    }
    if !value.is_finite() || value < 0.0 {
        return Err(ProfileError::BadValue {
            key: key.to_string(),
            value,
        });
    }
    Ok(())
}

impl Profile {
    /// Check every key and value against the field table.
    pub fn validate(&self) -> Result<(), ProfileError> {
        for (k, v) in &self.avg {
            check_value(k, *v)?;
        }
        for (k, cols) in &self.pctiles {
            for (label, v) in cols {
                if !is_pctile_label(label) {
                    return Err(ProfileError::BadPick(label.clone()));
                }
                check_value(k, *v)?;
            }
        }
        for k in self.provenance.keys() {
            if field(k).is_none() {
                return Err(ProfileError::UnknownField(k.clone()));
            }
        }
        Ok(())
    }

    /// Parse and validate a profile from JSON.
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        let profile: Profile = serde_json::from_str(text)?;
        profile.validate()?;
        Ok(profile)
    }

    /// Serialize with stable key order and a trailing newline.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("profile serializes");
        s.push('\n');
        s
    }

    /// The flat target map for one column. Fields absent from that
    /// column are left out, not zeroed, except static fields (hints and
    /// provenance), which have no percentiles and are taken from `avg`.
    pub fn pick(&self, pick: &Pick) -> Result<Targets, ProfileError> {
        let mut values: BTreeMap<String, f64> = match pick {
            Pick::Avg => self.avg.clone(),
            Pick::Pctile(label) => self
                .pctiles
                .iter()
                .filter_map(|(k, cols)| cols.get(label).map(|v| (k.clone(), *v)))
                .collect(),
        };
        if values.is_empty() {
            return Err(ProfileError::MissingColumn(pick.to_string()));
        }
        for f in FIELDS.iter().filter(|f| f.agg == Agg::Static) {
            if let Some(v) = self.avg.get(f.key) {
                values.entry(f.key.to_string()).or_insert(*v);
            }
        }
        Ok(Targets { values })
    }
}

/// A flat map of field to requested value, the input to `run`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Targets {
    values: BTreeMap<String, f64>,
}

impl Targets {
    /// An empty target set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one field, validating key and value.
    pub fn set(&mut self, key: &str, value: f64) -> Result<(), ProfileError> {
        check_value(key, value)?;
        self.values.insert(key.to_string(), value);
        Ok(())
    }

    /// Apply a `key=value` override as typed on the command line.
    pub fn set_from_arg(&mut self, arg: &str) -> Result<(), ProfileError> {
        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| ProfileError::BadOverride(arg.to_string()))?;
        let value: f64 = value
            .trim()
            .parse()
            .map_err(|_| ProfileError::BadOverride(arg.to_string()))?;
        self.set(key.trim(), value)
    }

    /// Value of a field, if requested.
    pub fn get(&self, key: &str) -> Option<f64> {
        self.values.get(key).copied()
    }

    /// Every requested field, in field-table order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static FieldSpec, f64)> + '_ {
        FIELDS
            .iter()
            .filter_map(|f| self.values.get(f.key).map(|v| (f, *v)))
    }

    /// Requested fields with [`Role::Target`], in table order.
    pub fn targets(&self) -> impl Iterator<Item = (&'static FieldSpec, f64)> + '_ {
        self.iter().filter(|(f, _)| f.role == Role::Target)
    }

    /// Number of requested fields.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether nothing was requested.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_keys_are_unique_and_ratios_reference_known_rates() {
        let mut seen = std::collections::HashSet::new();
        for f in FIELDS {
            assert!(seen.insert(f.key), "duplicate key {}", f.key);
            if let Agg::RatioOfSums { num, den, .. } = f.agg {
                assert_eq!(
                    field(num).map(|s| s.agg),
                    Some(Agg::Mean),
                    "{num} must be a rate"
                );
                assert_eq!(
                    field(den).map(|s| s.agg),
                    Some(Agg::Mean),
                    "{den} must be a rate"
                );
            }
        }
    }

    #[test]
    fn every_target_has_a_unit_or_is_dimensionless() {
        for f in FIELDS.iter().filter(|f| f.role == Role::Target) {
            let dimensionless = ["ipc", "cpu_usage_cv_1s"];
            assert!(
                !f.unit.is_empty() || dimensionless.contains(&f.key),
                "{} has no unit",
                f.key
            );
        }
    }

    #[test]
    fn pick_labels() {
        assert_eq!("avg".parse::<Pick>(), Ok(Pick::Avg));
        assert_eq!("p80".parse::<Pick>(), Ok(Pick::Pctile("p80".into())));
        assert_eq!("p99.9".parse::<Pick>(), Ok(Pick::Pctile("p99.9".into())));
        assert_eq!(
            "p101".parse::<Pick>(),
            Err(ProfileError::BadPick("p101".into()))
        );
        assert_eq!(
            "median".parse::<Pick>(),
            Err(ProfileError::BadPick("median".into()))
        );
        assert_eq!(Pick::Pctile("p5".into()).to_string(), "p5");
    }

    #[test]
    fn fixture_roundtrips_and_picks() {
        let text = include_str!("../tests/fixtures/profile.json");
        let p = Profile::from_json(text).unwrap();
        assert_eq!(p.count, 1000);
        assert_eq!(p.cpu.as_ref().unwrap().vendor, "amd");
        let again = Profile::from_json(&p.to_json()).unwrap();
        assert_eq!(p, again);

        let avg = p.pick(&Pick::Avg).unwrap();
        assert_eq!(avg.get("cpu_usage_cores"), Some(4.0));
        assert_eq!(avg.targets().count(), 11);
        assert_eq!(avg.iter().count(), avg.len());

        let p80 = p.pick(&Pick::Pctile("p80".into())).unwrap();
        assert_eq!(p80.get("ipc"), Some(1.6));
        assert_eq!(p80.get("cpu_limit_cores"), Some(8.0), "hints come from avg");
        assert_eq!(p80.get("tlb_host_minutes"), Some(48000.0));
        assert_eq!(
            p80.get("instructions_per_sec"),
            None,
            "no percentile recorded"
        );

        assert_eq!(
            p.pick(&Pick::Pctile("p42".into())),
            Err(ProfileError::MissingColumn("p42".into()))
        );
    }

    #[test]
    fn validation_rejects_unknown_keys_and_bad_values() {
        let mut p = Profile::default();
        p.avg.insert("ipc".into(), 1.0);
        p.validate().unwrap();

        p.avg.insert("bogus".into(), 1.0);
        assert_eq!(
            p.validate(),
            Err(ProfileError::UnknownField("bogus".into()))
        );
        p.avg.remove("bogus");

        p.avg.insert("ipc".into(), -1.0);
        assert_eq!(
            p.validate(),
            Err(ProfileError::BadValue {
                key: "ipc".into(),
                value: -1.0
            })
        );
        p.avg.insert("ipc".into(), 1.0);

        p.pctiles
            .insert("ipc".into(), BTreeMap::from([("median".to_string(), 1.0)]));
        assert_eq!(p.validate(), Err(ProfileError::BadPick("median".into())));
        p.pctiles.clear();

        p.provenance.insert(
            "nope".into(),
            Provenance {
                scope: Scope::Host,
                mode: Mode::All,
                method: Method::Perf,
            },
        );
        assert_eq!(p.validate(), Err(ProfileError::UnknownField("nope".into())));
    }

    #[test]
    fn from_json_rejects_nan_and_unknown_fields() {
        assert!(Profile::from_json(r#"{"count":1,"avg":{"ipc":NaN}}"#).is_err());
        assert!(Profile::from_json(r#"{"count":1,"avg":{"nope":1}}"#).is_err());
        assert!(Profile::from_json(r#"{"count":1,"avg":{}}"#).is_ok());
    }

    #[test]
    fn overrides() {
        let mut t = Targets::new();
        assert!(t.is_empty());
        t.set_from_arg("ipc=1.5").unwrap();
        t.set_from_arg(" l3_mpki = 2 ").unwrap();
        assert_eq!(t.get("ipc"), Some(1.5));
        assert_eq!(t.get("l3_mpki"), Some(2.0));
        assert_eq!(
            t.set_from_arg("ipc"),
            Err(ProfileError::BadOverride("ipc".into()))
        );
        assert_eq!(
            t.set_from_arg("ipc=fast"),
            Err(ProfileError::BadOverride("ipc=fast".into()))
        );
        assert_eq!(
            t.set_from_arg("nope=1"),
            Err(ProfileError::UnknownField("nope".into()))
        );
        assert_eq!(
            t.set("ipc", f64::INFINITY),
            Err(ProfileError::BadValue {
                key: "ipc".into(),
                value: f64::INFINITY
            })
        );
        let keys: Vec<_> = t.iter().map(|(f, _)| f.key).collect();
        assert_eq!(keys, ["ipc", "l3_mpki"], "table order, not insertion order");
    }

    #[test]
    fn provenance_serializes_snake_case() {
        let p = Provenance {
            scope: Scope::Cgroup,
            mode: Mode::NotApplicable,
            method: Method::Cachestat,
        };
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"scope":"cgroup","mode":"not_applicable","method":"cachestat"}"#
        );
    }

    #[test]
    fn role_listing() {
        let hints: Vec<_> = keys_with_role(Role::Hint).collect();
        assert_eq!(hints, ["cpu_limit_cores", "memory_limit_bytes"]);
        assert_eq!(keys_with_role(Role::Target).count(), 11);
    }
}
