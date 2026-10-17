//! Plots for the experiments: shared by `experiment_plot` and its test.
//! Static SVG through plotters; colours are the first categorical slots
//! of the reference palette in fixed order, never cycled.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use plotters::prelude::*;
use plotters::style::text_anchor::{HPos, Pos, VPos};
use thrashgen::compare::Comparison;
use thrashgen::samples::Recording;

/// One experiment's inputs.
pub struct Experiment {
    /// Directory name.
    pub name: String,
    /// Recording of the benchmark.
    pub orig: Recording,
    /// Recording of the replay.
    pub replay: Recording,
    /// Field comparison, benchmark against replay.
    pub compare: Comparison,
}

/// Categorical slots: benchmark, replay, then further experiments.
pub const SERIES: [RGBColor; 4] = [
    RGBColor(0x2a, 0x78, 0xd6),
    RGBColor(0xeb, 0x68, 0x34),
    RGBColor(0x1b, 0xaf, 0x7a),
    RGBColor(0xed, 0xa1, 0x00),
];
const INK: RGBColor = RGBColor(0x33, 0x33, 0x30);
const MUTED: RGBColor = RGBColor(0x8a, 0x89, 0x80);
const SURFACE: RGBColor = RGBColor(0xfc, 0xfc, 0xfb);

type Res = Result<(), Box<dyn std::error::Error>>;

/// Load an experiment directory written by `experiments/run.sh`.
pub fn load(dir: &Path) -> Result<Experiment, Box<dyn std::error::Error>> {
    let rec = |f: &str| -> Result<Recording, Box<dyn std::error::Error>> {
        Ok(Recording::read(BufReader::new(File::open(dir.join(f))?))?)
    };
    Ok(Experiment {
        name: dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        orig: rec("orig.jsonl")?,
        replay: rec("replay.jsonl")?,
        compare: serde_json::from_str(&std::fs::read_to_string(dir.join("compare.json"))?)?,
    })
}

fn font(size: u32, color: &'static RGBColor) -> TextStyle<'static> {
    TextStyle::from(("sans-serif", size).into_font()).color(color)
}

/// Replay over benchmark per observable, with a reference line at one.
pub fn ratio_chart(exp: &Experiment, path: &Path) -> Res {
    let rows: Vec<(&str, f64)> = exp
        .compare
        .rows
        .iter()
        .filter_map(|r| {
            r.actual.map(|a| {
                (
                    r.key.as_str(),
                    if r.expected > 0.0 {
                        a / r.expected
                    } else {
                        f64::NAN
                    },
                )
            })
        })
        .filter(|(_, v)| v.is_finite())
        .collect();
    let n = rows.len().max(1);
    let max = rows.iter().map(|(_, v)| *v).fold(1.0f64, f64::max).min(4.0) * 1.15;
    let root = SVGBackend::new(path, (720, 60 + 28 * n as u32)).into_drawing_area();
    root.fill(&SURFACE)?;
    let mut chart = ChartBuilder::on(&root)
        .caption(
            format!("{}: replay over benchmark", exp.name),
            font(15, &INK),
        )
        .margin(12)
        .x_label_area_size(28)
        .y_label_area_size(150)
        .build_cartesian_2d(0.0..max, 0.0..n as f64)?;
    chart
        .configure_mesh()
        .disable_mesh()
        .y_labels(0)
        .x_labels(5)
        .x_desc("ratio")
        .axis_desc_style(font(11, &MUTED))
        .label_style(font(11, &MUTED))
        .axis_style(MUTED.stroke_width(1))
        .draw()?;
    chart.draw_series(std::iter::once(PathElement::new(
        vec![(1.0, 0.0), (1.0, n as f64)],
        MUTED.stroke_width(1),
    )))?;
    for (i, (key, ratio)) in rows.iter().enumerate() {
        let y = (n - 1 - i) as f64;
        let shown = ratio.min(max);
        chart.draw_series(std::iter::once(Rectangle::new(
            [(0.0, y + 0.25), (shown, y + 0.75)],
            SERIES[1].filled(),
        )))?;
        chart.draw_series(std::iter::once(Text::new(
            key.to_string(),
            (0.0, y + 0.5),
            font(11, &INK).pos(Pos::new(HPos::Right, VPos::Center)),
        )))?;
        let label = if *ratio > max {
            format!("{ratio:.2}x")
        } else {
            format!("{:+.1}%", (ratio - 1.0) * 100.0)
        };
        chart.draw_series(std::iter::once(Text::new(
            label,
            (shown + max * 0.01, y + 0.5),
            font(11, &INK).pos(Pos::new(HPos::Left, VPos::Center)),
        )))?;
    }
    root.present()?;
    Ok(())
}

fn series(rec: &Recording) -> Vec<(f64, f64)> {
    let t0 = rec.cpu.first().map_or(0.0, |c| c.t);
    rec.cpu.iter().map(|c| (c.t - t0, c.cores)).collect()
}

/// Busy cores at 1 Hz for the benchmark and the replay, on one axis.
pub fn cores_chart(exp: &Experiment, path: &Path) -> Res {
    let (a, b) = (series(&exp.orig), series(&exp.replay));
    let x_max = a.iter().chain(&b).map(|p| p.0).fold(1.0, f64::max);
    let y_max = a.iter().chain(&b).map(|p| p.1).fold(0.5, f64::max) * 1.1;
    let root = SVGBackend::new(path, (720, 300)).into_drawing_area();
    root.fill(&SURFACE)?;
    let mut chart = ChartBuilder::on(&root)
        .caption(
            format!("{}: busy cores, one-second samples", exp.name),
            font(15, &INK),
        )
        .margin(12)
        .x_label_area_size(28)
        .y_label_area_size(44)
        .build_cartesian_2d(0.0..x_max, 0.0..y_max)?;
    chart
        .configure_mesh()
        .disable_x_mesh()
        .light_line_style(RGBColor(0xe6, 0xe5, 0xe0))
        .x_desc("seconds")
        .y_desc("cores")
        .axis_desc_style(font(11, &MUTED))
        .label_style(font(11, &MUTED))
        .axis_style(MUTED.stroke_width(1))
        .draw()?;
    for (data, name, color) in [(&a, "benchmark", SERIES[0]), (&b, "replay", SERIES[1])] {
        chart
            .draw_series(LineSeries::new(data.iter().copied(), color.stroke_width(2)))?
            .label(name)
            .legend(move |(x, y)| {
                PathElement::new(vec![(x, y), (x + 18, y)], color.stroke_width(2))
            });
    }
    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::UpperRight)
        .label_font(font(11, &INK))
        .background_style(SURFACE.filled())
        .border_style(MUTED.stroke_width(1))
        .draw()?;
    root.present()?;
    Ok(())
}

/// Every experiment's ratios on one chart, grouped by observable.
pub fn summary_chart(exps: &[Experiment], path: &Path) -> Res {
    let keys: Vec<String> = {
        let mut k: Vec<String> = Vec::new();
        for e in exps {
            for r in &e.compare.rows {
                if r.actual.is_some() && !k.contains(&r.key) {
                    k.push(r.key.clone());
                }
            }
        }
        k
    };
    let n = keys.len().max(1);
    let m = exps.len().max(1);
    let ratio = |e: &Experiment, key: &str| {
        e.compare
            .rows
            .iter()
            .find(|r| r.key == key)
            .and_then(|r| r.actual.map(|a| a / r.expected))
            .filter(|v| v.is_finite())
    };
    let max = exps
        .iter()
        .flat_map(|e| keys.iter().filter_map(move |k| ratio(e, k)))
        .fold(1.0f64, f64::max)
        .min(4.0)
        * 1.15;
    let root = SVGBackend::new(path, (900, 60 + 24 * (n * m) as u32)).into_drawing_area();
    root.fill(&SURFACE)?;
    let mut chart = ChartBuilder::on(&root)
        .caption("replay over benchmark, every experiment", font(15, &INK))
        .margin(12)
        .x_label_area_size(28)
        .y_label_area_size(150)
        .build_cartesian_2d(0.0..max, 0.0..n as f64)?;
    chart
        .configure_mesh()
        .disable_mesh()
        .y_labels(0)
        .x_labels(5)
        .x_desc("ratio")
        .axis_desc_style(font(11, &MUTED))
        .label_style(font(11, &MUTED))
        .axis_style(MUTED.stroke_width(1))
        .draw()?;
    chart.draw_series(std::iter::once(PathElement::new(
        vec![(1.0, 0.0), (1.0, n as f64)],
        MUTED.stroke_width(1),
    )))?;
    let slot = 0.8 / m as f64;
    for (ei, e) in exps.iter().enumerate() {
        let color = SERIES[ei % SERIES.len()];
        let bars: Vec<Rectangle<(f64, f64)>> = keys
            .iter()
            .enumerate()
            .filter_map(|(ki, key)| {
                let r = ratio(e, key)?;
                let y = (n - 1 - ki) as f64 + 0.1 + slot * ei as f64;
                Some(Rectangle::new(
                    [(0.0, y), (r.min(max), y + slot * 0.9)],
                    color.filled(),
                ))
            })
            .collect();
        chart
            .draw_series(bars)?
            .label(e.name.clone())
            .legend(move |(x, y)| Rectangle::new([(x, y - 5), (x + 14, y + 5)], color.filled()));
    }
    for (ki, key) in keys.iter().enumerate() {
        chart.draw_series(std::iter::once(Text::new(
            key.clone(),
            (0.0, (n - 1 - ki) as f64 + 0.5),
            font(11, &INK).pos(Pos::new(HPos::Right, VPos::Center)),
        )))?;
    }
    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::LowerRight)
        .label_font(font(11, &INK))
        .background_style(SURFACE.filled())
        .border_style(MUTED.stroke_width(1))
        .draw()?;
    root.present()?;
    Ok(())
}

/// Draw every chart for the experiments under `out`.
pub fn draw_all(out: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut exps = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(out)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("compare.json").is_file())
        .collect();
    entries.sort();
    for dir in entries {
        let exp = load(&dir)?;
        ratio_chart(&exp, &dir.join("ratio.svg"))?;
        cores_chart(&exp, &dir.join("cores.svg"))?;
        exps.push(exp);
    }
    if !exps.is_empty() {
        summary_chart(&exps, &out.join("summary.svg"))?;
    }
    Ok(exps.into_iter().map(|e| e.name).collect())
}
