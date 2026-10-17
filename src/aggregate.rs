//! Build a profile from recordings: `docs/design.md` section 3.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::profile::{Agg, CpuIdent, FIELDS, Profile, Provenance, Role};
use crate::samples::{CpuSample, Recording, Sample};

/// Tunables for [`aggregate`].
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// Window length for the burstiness statistic, seconds.
    pub window_s: f64,
    /// Percentiles to emit, in 0..=100.
    pub pctiles: Vec<f64>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            window_s: 300.0,
            pctiles: vec![1.0, 5.0, 10.0, 25.0, 50.0, 75.0, 90.0, 95.0, 99.0],
        }
    }
}

/// Errors from [`aggregate`].
#[derive(Debug, Error, PartialEq)]
pub enum AggregateError {
    /// Nothing to aggregate.
    #[error("no recordings")]
    NoRecordings,
    /// Recordings exist but hold no samples at all.
    #[error("recordings contain no samples")]
    NoSamples,
    /// Two recordings name different CPUs.
    #[error("recordings come from different CPUs: {0} and {1}")]
    CpuMismatch(String, String),
    /// Two recordings measured a field differently.
    #[error("field `{0}` has different provenance across recordings")]
    ProvenanceMismatch(String),
    /// Window or percentile out of range.
    #[error("bad option: {0}")]
    BadOption(String),
}

/// Linear-interpolation percentile of sorted values (NumPy's default).
/// `p` is in 0..=100. Panics on an empty slice.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    assert!(!sorted.is_empty(), "percentile of nothing");
    let rank = p / 100.0 * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

/// Mean and population variance of a window.
fn moments(xs: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    let m = xs.iter().sum::<f64>() / n;
    let v = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n;
    (m, v)
}

/// Split one scope's 1 Hz series into windows of `per_window` samples.
/// A trailing partial window is kept when it is at least half full or
/// is the only window, so short recordings still yield a value.
fn windows(series: &[f64], per_window: usize) -> Vec<&[f64]> {
    let per_window = per_window.max(1);
    let mut out: Vec<&[f64]> = Vec::new();
    for chunk in series.chunks(per_window) {
        if chunk.len() == per_window || out.is_empty() || chunk.len() * 2 >= per_window {
            out.push(chunk);
        }
    }
    out
}

/// Pooled coefficient of variation over all windows of all scopes:
/// `sqrt(mean variance) / mean mean`. Also returns each window's own
/// CV, `sqrt(v) / m`, for windows with a positive mean. `None` when
/// there is no data or every window has mean zero.
pub fn pooled_cv(series_per_scope: &[Vec<f64>], per_window: usize) -> Option<(f64, Vec<f64>)> {
    let mut means = Vec::new();
    let mut vars = Vec::new();
    let mut per_window_cv = Vec::new();
    for series in series_per_scope {
        if series.is_empty() {
            continue;
        }
        for w in windows(series, per_window) {
            let (m, v) = moments(w);
            means.push(m);
            vars.push(v);
            if m > 0.0 {
                per_window_cv.push(v.sqrt() / m);
            }
        }
    }
    if means.is_empty() {
        return None;
    }
    let mean_m = means.iter().sum::<f64>() / means.len() as f64;
    if mean_m <= 0.0 {
        return None;
    }
    let mean_v = vars.iter().sum::<f64>() / vars.len() as f64;
    Some((mean_v.sqrt() / mean_m, per_window_cv))
}

/// Label for a percentile: `p50`, `p99.9`.
pub fn pctile_label(p: f64) -> String {
    if p.fract() == 0.0 {
        format!("p{}", p as u32)
    } else {
        format!("p{p}")
    }
}

type ScopeKey = (usize, String);

fn check_options(opts: &Options) -> Result<(), AggregateError> {
    if opts.window_s <= 0.0 || !opts.window_s.is_finite() {
        return Err(AggregateError::BadOption(format!(
            "window {} s",
            opts.window_s
        )));
    }
    if let Some(p) = opts.pctiles.iter().find(|p| !(0.0..=100.0).contains(*p)) {
        return Err(AggregateError::BadOption(format!("percentile {p}")));
    }
    Ok(())
}

fn merge_cpu(recordings: &[Recording]) -> Result<Option<CpuIdent>, AggregateError> {
    let mut first: Option<&CpuIdent> = None;
    for c in recordings.iter().filter_map(|r| r.header.cpu.as_ref()) {
        match first {
            None => first = Some(c),
            Some(f)
                if (f.vendor.as_str(), f.family, f.model)
                    != (c.vendor.as_str(), c.family, c.model) =>
            {
                return Err(AggregateError::CpuMismatch(f.name.clone(), c.name.clone()));
            }
            Some(_) => {}
        }
    }
    Ok(first.cloned())
}

fn merge_provenance(
    recordings: &[Recording],
) -> Result<BTreeMap<String, Provenance>, AggregateError> {
    let mut merged: BTreeMap<String, Provenance> = BTreeMap::new();
    for r in recordings {
        // Diagnostic keys the recorder notes alongside the fields, such
        // as the DRAM-fill proxy next to L3 PMU misses, have no field
        // and the profile would not validate with them.
        for (k, p) in r
            .header
            .provenance
            .iter()
            .filter(|(k, _)| crate::profile::field(k).is_some())
        {
            match merged.get(k) {
                Some(existing) if existing != p => {
                    return Err(AggregateError::ProvenanceMismatch(k.clone()));
                }
                Some(_) => {}
                None => {
                    merged.insert(k.clone(), *p);
                }
            }
        }
    }
    Ok(merged)
}

/// Per-scope dt-weighted means of one field, over scopes that have it.
fn scope_means(samples: &[(ScopeKey, &Sample)], key: &str) -> Vec<f64> {
    let mut acc: BTreeMap<&ScopeKey, (f64, f64)> = BTreeMap::new();
    for (scope, s) in samples {
        if let Some(v) = s.values.get(key) {
            let e = acc.entry(scope).or_insert((0.0, 0.0));
            e.0 += v * s.dt;
            e.1 += s.dt;
        }
    }
    acc.values()
        .filter(|(_, w)| *w > 0.0)
        .map(|(sum, w)| sum / w)
        .collect()
}

fn mean(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        None
    } else {
        Some(xs.iter().sum::<f64>() / xs.len() as f64)
    }
}

fn pctiles_of(mut values: Vec<f64>, opts: &Options) -> Option<BTreeMap<String, f64>> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    Some(
        opts.pctiles
            .iter()
            .map(|p| (pctile_label(*p), percentile(&values, *p)))
            .collect(),
    )
}

/// Combine recordings into a profile. Scopes are identified by
/// recording and scope name, so the same cgroup recorded twice counts
/// as two containers.
pub fn aggregate(recordings: &[Recording], opts: &Options) -> Result<Profile, AggregateError> {
    check_options(opts)?;
    if recordings.is_empty() {
        return Err(AggregateError::NoRecordings);
    }
    let samples: Vec<(ScopeKey, &Sample)> = recordings
        .iter()
        .enumerate()
        .flat_map(|(i, r)| r.samples.iter().map(move |s| ((i, s.scope.clone()), s)))
        .collect();
    let cpu_samples: Vec<(ScopeKey, &CpuSample)> = recordings
        .iter()
        .enumerate()
        .flat_map(|(i, r)| r.cpu.iter().map(move |c| ((i, c.scope.clone()), c)))
        .collect();
    if samples.is_empty() && cpu_samples.is_empty() {
        return Err(AggregateError::NoSamples);
    }

    let mut profile = Profile {
        count: 0,
        cpu: merge_cpu(recordings)?,
        avg: BTreeMap::new(),
        pctiles: BTreeMap::new(),
        provenance: merge_provenance(recordings)?,
    };
    let mut scopes: Vec<&ScopeKey> = samples
        .iter()
        .map(|(k, _)| k)
        .chain(cpu_samples.iter().map(|(k, _)| k))
        .collect();
    scopes.sort();
    scopes.dedup();
    profile.count = scopes.len() as u64;

    for f in FIELDS {
        match f.agg {
            Agg::Mean | Agg::Static => {
                if let Some(avg) = mean(&scope_means(&samples, f.key)) {
                    profile.avg.insert(f.key.to_string(), avg);
                }
                if f.role != Role::Hint && f.role != Role::Provenance {
                    let all: Vec<f64> = samples
                        .iter()
                        .filter_map(|(_, s)| s.values.get(f.key).copied())
                        .collect();
                    if let Some(p) = pctiles_of(all, opts) {
                        profile.pctiles.insert(f.key.to_string(), p);
                    }
                }
            }
            Agg::RatioOfSums { num, den, scale } => {
                let (mut sum_num, mut sum_den) = (0.0, 0.0);
                let mut per_sample = Vec::new();
                for (_, s) in &samples {
                    if let (Some(n), Some(d)) = (s.values.get(num), s.values.get(den)) {
                        sum_num += n * s.dt;
                        sum_den += d * s.dt;
                        if *d > 0.0 {
                            per_sample.push(n / d * scale);
                        }
                    }
                }
                if sum_den > 0.0 {
                    profile
                        .avg
                        .insert(f.key.to_string(), sum_num / sum_den * scale);
                }
                if let Some(p) = pctiles_of(per_sample, opts) {
                    profile.pctiles.insert(f.key.to_string(), p);
                }
            }
            Agg::PooledCv => {
                let mut by_scope: BTreeMap<&ScopeKey, Vec<(f64, f64)>> = BTreeMap::new();
                for (k, c) in &cpu_samples {
                    by_scope.entry(k).or_default().push((c.t, c.cores));
                }
                let series: Vec<Vec<f64>> = by_scope
                    .into_values()
                    .map(|mut v| {
                        v.sort_by(|a, b| a.0.total_cmp(&b.0));
                        v.into_iter().map(|(_, cores)| cores).collect()
                    })
                    .collect();
                let cpu_interval = recordings
                    .iter()
                    .map(|r| r.header.cpu_interval_s)
                    .find(|i| *i > 0.0)
                    .unwrap_or(1.0);
                let per_window = (opts.window_s / cpu_interval).round().max(1.0) as usize;
                if let Some((cv, per_window_cv)) = pooled_cv(&series, per_window) {
                    profile.avg.insert(f.key.to_string(), cv);
                    if let Some(p) = pctiles_of(per_window_cv, opts) {
                        profile.pctiles.insert(f.key.to_string(), p);
                    }
                }
            }
        }
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Method, Mode, Pick, Scope};
    use crate::samples::{FORMAT_VERSION, Header, Target};
    use proptest::prelude::*;

    fn header(cpu: Option<CpuIdent>) -> Header {
        Header {
            version: FORMAT_VERSION,
            host: "box".into(),
            kernel: "7.1.0".into(),
            cpu,
            started_unix_s: 0.0,
            interval_s: 60.0,
            cpu_interval_s: 1.0,
            target: Target::Host,
            provenance: BTreeMap::new(),
        }
    }

    fn sample(scope: &str, dt: f64, kv: &[(&str, f64)]) -> Sample {
        Sample {
            t: 0.0,
            scope: scope.into(),
            dt,
            values: kv.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    fn cpu(scope: &str, series: &[f64]) -> Vec<CpuSample> {
        series
            .iter()
            .enumerate()
            .map(|(i, c)| CpuSample {
                t: i as f64,
                scope: scope.into(),
                cores: *c,
            })
            .collect()
    }

    fn rec(samples: Vec<Sample>, cpu: Vec<CpuSample>) -> Recording {
        Recording {
            header: header(None),
            samples,
            cpu,
        }
    }

    #[test]
    fn percentile_interpolates_linearly() {
        let v = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 100.0), 4.0);
        assert_eq!(percentile(&v, 50.0), 2.5);
        assert_eq!(percentile(&v, 25.0), 1.75);
        assert_eq!(percentile(&[7.0], 99.0), 7.0);
    }

    #[test]
    #[should_panic(expected = "percentile of nothing")]
    fn percentile_of_empty_panics() {
        percentile(&[], 50.0);
    }

    #[test]
    fn labels() {
        assert_eq!(pctile_label(50.0), "p50");
        assert_eq!(pctile_label(99.9), "p99.9");
        assert_eq!(pctile_label(0.0), "p0");
    }

    #[test]
    fn windows_keep_half_full_tail_or_only_window() {
        let s: Vec<f64> = (0..10).map(|i| i as f64).collect();
        assert_eq!(windows(&s, 4).len(), 3, "4,4,2: tail is half full");
        assert_eq!(windows(&s, 7).len(), 1, "7,3: tail under half is dropped");
        assert_eq!(
            windows(&s, 20).len(),
            1,
            "shorter than a window: whole series"
        );
        assert_eq!(windows(&s, 0).len(), 10, "zero is clamped to one");
    }

    #[test]
    fn pooled_cv_cases() {
        assert_eq!(pooled_cv(&[vec![2.0; 10]], 5), Some((0.0, vec![0.0, 0.0])));
        // window [0,2]: m=1, v=1 -> cv 1
        let (cv, per) = pooled_cv(&[vec![0.0, 2.0]], 2).unwrap();
        assert!((cv - 1.0).abs() < 1e-12);
        assert_eq!(per, vec![1.0]);
        // pooled: scope A m=1 v=1, scope B m=3 v=0 -> sqrt(0.5)/2
        let (cv, per) = pooled_cv(&[vec![0.0, 2.0], vec![3.0, 3.0]], 2).unwrap();
        assert!((cv - 0.5_f64.sqrt() / 2.0).abs() < 1e-12);
        assert_eq!(per.len(), 2);
        // an idle scope contributes a zero-mean window but no per-window cv
        let (cv, per) = pooled_cv(&[vec![0.0, 0.0], vec![1.0, 1.0]], 2).unwrap();
        assert_eq!(cv, 0.0);
        assert_eq!(per, vec![0.0]);
        assert_eq!(pooled_cv(&[vec![0.0, 0.0]], 2), None);
        assert_eq!(pooled_cv(&[vec![]], 2), None);
        assert_eq!(pooled_cv(&[], 2), None);
    }

    #[test]
    fn scopes_weigh_equally_and_samples_weigh_by_dt() {
        let r = rec(
            vec![
                sample("a", 10.0, &[("anon_bytes", 1.0)]),
                sample("a", 30.0, &[("anon_bytes", 3.0)]),
                sample("b", 60.0, &[("anon_bytes", 10.0)]),
            ],
            vec![],
        );
        let p = aggregate(&[r], &Options::default()).unwrap();
        assert_eq!(p.count, 2);
        // a: (1*10 + 3*30)/40 = 2.5; b: 10; equal weight -> 6.25
        assert_eq!(p.avg["anon_bytes"], 6.25);
        // percentiles over all three samples
        assert_eq!(p.pctiles["anon_bytes"]["p50"], 3.0);
        assert_eq!(p.pctiles["anon_bytes"]["p1"], 1.04);
    }

    #[test]
    fn ratios_are_ratio_of_sums() {
        let r = rec(
            vec![
                sample(
                    "a",
                    60.0,
                    &[("instructions_per_sec", 10.0), ("cycles_per_sec", 10.0)],
                ),
                sample(
                    "b",
                    60.0,
                    &[("instructions_per_sec", 90.0), ("cycles_per_sec", 30.0)],
                ),
                sample("c", 60.0, &[("instructions_per_sec", 5.0)]),
            ],
            vec![],
        );
        let p = aggregate(
            &[r],
            &Options {
                pctiles: vec![0.0, 50.0, 100.0],
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(p.avg["ipc"], 2.5, "100/40, not the mean of 1 and 3");
        assert_eq!(p.pctiles["ipc"]["p0"], 1.0);
        assert_eq!(p.pctiles["ipc"]["p100"], 3.0);
        assert_eq!(p.avg["instructions_per_sec"], 35.0, "(10+90+5)/3");
        assert!(!p.avg.contains_key("l3_mpki"), "no misses recorded");
    }

    #[test]
    fn ratio_scale_and_zero_denominator() {
        let r = rec(
            vec![
                sample(
                    "a",
                    1.0,
                    &[("instructions_per_sec", 2000.0), ("l3_misses_per_sec", 4.0)],
                ),
                sample(
                    "a",
                    1.0,
                    &[("instructions_per_sec", 0.0), ("l3_misses_per_sec", 1.0)],
                ),
            ],
            vec![],
        );
        let p = aggregate(&[r], &Options::default()).unwrap();
        assert_eq!(
            p.avg["l3_mpki"], 2.5,
            "5 misses per 2000 instructions, per kilo"
        );
        assert_eq!(
            p.pctiles["l3_mpki"]["p50"], 2.0,
            "the zero-denominator sample has no ratio"
        );
    }

    #[test]
    fn burstiness_from_cpu_samples() {
        let r = rec(vec![], cpu("a", &[0.0, 2.0, 0.0, 2.0]));
        let p = aggregate(
            &[r],
            &Options {
                window_s: 2.0,
                pctiles: vec![50.0],
            },
        )
        .unwrap();
        assert!((p.avg["cpu_usage_cv_1s"] - 1.0).abs() < 1e-12);
        assert_eq!(p.pctiles["cpu_usage_cv_1s"]["p50"], 1.0);
        assert_eq!(p.count, 1);
        assert!(
            !p.avg.contains_key("cpu_usage_cores"),
            "no interval samples"
        );
    }

    #[test]
    fn hints_average_without_percentiles() {
        let r = rec(
            vec![
                sample("a", 60.0, &[("cpu_limit_cores", 4.0)]),
                sample("b", 60.0, &[("cpu_limit_cores", 8.0)]),
            ],
            vec![],
        );
        let p = aggregate(&[r], &Options::default()).unwrap();
        assert_eq!(p.avg["cpu_limit_cores"], 6.0);
        assert!(!p.pctiles.contains_key("cpu_limit_cores"));
        assert_eq!(
            p.pick(&Pick::Avg).unwrap().get("cpu_limit_cores"),
            Some(6.0)
        );
    }

    #[test]
    fn same_scope_in_two_recordings_counts_twice() {
        let a = rec(vec![sample("x", 60.0, &[("anon_bytes", 1.0)])], vec![]);
        let b = rec(vec![sample("x", 60.0, &[("anon_bytes", 3.0)])], vec![]);
        let p = aggregate(&[a, b], &Options::default()).unwrap();
        assert_eq!(p.count, 2);
        assert_eq!(p.avg["anon_bytes"], 2.0);
    }

    #[test]
    fn cpu_and_provenance_merge_or_conflict() {
        let amd = CpuIdent {
            vendor: "amd".into(),
            family: 25,
            model: 24,
            name: "A".into(),
        };
        let amd2 = CpuIdent {
            name: "A2".into(),
            ..amd.clone()
        };
        let intel = CpuIdent {
            vendor: "intel".into(),
            family: 6,
            model: 173,
            name: "I".into(),
        };
        let prov = Provenance {
            scope: Scope::Cgroup,
            mode: Mode::All,
            method: Method::Perf,
        };
        let other = Provenance {
            mode: Mode::User,
            ..prov
        };

        let mut a = rec(vec![sample("x", 1.0, &[("ipc", 1.0)])], vec![]);
        a.header.cpu = Some(amd.clone());
        a.header.provenance.insert("ipc".into(), prov);
        let mut b = rec(vec![sample("y", 1.0, &[("ipc", 1.0)])], vec![]);
        b.header.cpu = Some(amd2);
        b.header.provenance.insert("l3_mpki".into(), prov);
        b.header
            .provenance
            .insert(crate::plan::L3_FILLS_KEY.into(), prov);
        let p = aggregate(&[a.clone(), b.clone()], &Options::default()).unwrap();
        assert_eq!(
            p.cpu.as_ref().unwrap().name,
            "A",
            "same CPU, first name wins"
        );
        assert_eq!(p.provenance.len(), 2, "the diagnostic key is dropped");
        p.validate().unwrap();

        b.header.cpu = Some(intel);
        assert_eq!(
            aggregate(&[a.clone(), b.clone()], &Options::default()),
            Err(AggregateError::CpuMismatch("A".into(), "I".into()))
        );

        b.header.cpu = None;
        b.header.provenance.insert("ipc".into(), other);
        assert_eq!(
            aggregate(&[a, b], &Options::default()),
            Err(AggregateError::ProvenanceMismatch("ipc".into()))
        );
    }

    #[test]
    fn errors_and_options() {
        assert_eq!(
            aggregate(&[], &Options::default()),
            Err(AggregateError::NoRecordings)
        );
        assert_eq!(
            aggregate(&[rec(vec![], vec![])], &Options::default()),
            Err(AggregateError::NoSamples)
        );
        let r = rec(vec![sample("a", 1.0, &[("ipc", 1.0)])], vec![]);
        assert!(matches!(
            aggregate(
                std::slice::from_ref(&r),
                &Options {
                    window_s: 0.0,
                    ..Options::default()
                }
            ),
            Err(AggregateError::BadOption(_))
        ));
        assert!(matches!(
            aggregate(
                &[r],
                &Options {
                    pctiles: vec![101.0],
                    ..Options::default()
                }
            ),
            Err(AggregateError::BadOption(_))
        ));
    }

    #[test]
    fn output_validates_against_the_schema() {
        let r = rec(
            vec![sample(
                "a",
                60.0,
                &[
                    ("ipc", 1.0),
                    ("instructions_per_sec", 1.0),
                    ("cycles_per_sec", 2.0),
                ],
            )],
            cpu("a", &[1.0, 2.0]),
        );
        let p = aggregate(&[r], &Options::default()).unwrap();
        p.validate().unwrap();
        assert_eq!(Profile::from_json(&p.to_json()).unwrap(), p);
    }

    proptest! {
        #[test]
        fn percentile_is_bounded_and_monotone(
            mut v in proptest::collection::vec(-1e6..1e6f64, 1..50),
            p in 0.0..=100.0f64,
            q in 0.0..=100.0f64,
        ) {
            v.sort_by(|a, b| a.total_cmp(b));
            let (lo, hi) = (p.min(q), p.max(q));
            let a = percentile(&v, lo);
            let b = percentile(&v, hi);
            prop_assert!(a >= v[0] && b <= v[v.len() - 1]);
            prop_assert!(a <= b);
        }

        #[test]
        fn pooled_cv_is_nonnegative_and_finite(
            series in proptest::collection::vec(proptest::collection::vec(0.0..100.0f64, 1..40), 1..4),
            per_window in 1usize..20,
        ) {
            if let Some((cv, per)) = pooled_cv(&series, per_window) {
                prop_assert!(cv >= 0.0 && cv.is_finite());
                prop_assert!(per.iter().all(|c| *c >= 0.0 && c.is_finite()));
            }
        }

        #[test]
        fn recording_order_does_not_matter(
            vals in proptest::collection::vec(0.0..1e3f64, 2..8),
        ) {
            let recs: Vec<Recording> = vals
                .iter()
                .enumerate()
                .map(|(i, v)| rec(vec![sample(&format!("s{i}"), 60.0, &[("anon_bytes", *v), ("instructions_per_sec", *v), ("cycles_per_sec", 1.0)])], cpu("s", &[*v, 0.0])))
                .collect();
            let fwd = aggregate(&recs, &Options::default()).unwrap();
            let mut rev = recs.clone();
            rev.reverse();
            let back = aggregate(&rev, &Options::default()).unwrap();
            prop_assert_eq!(fwd.count, back.count);
            for (k, v) in &fwd.avg {
                prop_assert!((v - back.avg[k]).abs() <= 1e-9 * v.abs().max(1.0), "{}", k);
            }
        }
    }
}
