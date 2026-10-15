//! The experiment plots draw from fixture data.

#[path = "../examples/experiment_plot/plot_lib.rs"]
mod plot_lib;

use thrashgen::aggregate;
use thrashgen::compare;
use thrashgen::profile::Pick;

#[test]
fn plots_draw_from_fixtures() {
    let fixtures = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"));
    let out = tempfile::tempdir().unwrap();
    for name in ["alpha", "beta"] {
        let dir = out.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::copy(fixtures.join("recording.jsonl"), dir.join("orig.jsonl")).unwrap();
        std::fs::copy(fixtures.join("recording.jsonl"), dir.join("replay.jsonl")).unwrap();
        let rec = thrashgen::samples::Recording::read(std::io::BufReader::new(
            std::fs::File::open(dir.join("orig.jsonl")).unwrap(),
        ))
        .unwrap();
        let profile =
            aggregate::aggregate(std::slice::from_ref(&rec), &aggregate::Options::default())
                .unwrap();
        std::fs::write(dir.join("orig.json"), profile.to_json()).unwrap();
        let targets = profile.pick(&Pick::Avg).unwrap();
        let mut actual = profile.avg.clone();
        actual.insert("ipc".into(), 2.3);
        let cmp = compare::compare(&targets, &actual, 0.05);
        std::fs::write(dir.join("compare.json"), cmp.to_json()).unwrap();
    }
    std::fs::create_dir(out.path().join("not-an-experiment")).unwrap();
    let names = plot_lib::draw_all(out.path()).unwrap();
    assert_eq!(names, ["alpha", "beta"]);
    for name in names {
        for f in ["ratio.svg", "cores.svg"] {
            let svg = std::fs::read_to_string(out.path().join(&name).join(f)).unwrap();
            assert!(svg.contains("<svg"), "{name}/{f}");
            assert!(svg.contains("ipc") || f == "cores.svg", "{name}/{f}");
        }
        let ratio = std::fs::read_to_string(out.path().join(&name).join("ratio.svg")).unwrap();
        assert!(ratio.contains("+15.0%"), "ipc 2.3 over 2.0");
    }
    let summary = std::fs::read_to_string(out.path().join("summary.svg")).unwrap();
    assert!(summary.contains("alpha") && summary.contains("beta"));
    assert!(plot_lib::load(&out.path().join("not-an-experiment")).is_err());
    assert!(plot_lib::draw_all(&out.path().join("empty-missing")).is_err());
}
