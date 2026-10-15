//! Draw the experiment plots: `cargo run --example experiment_plot -- experiments/out`.

#[path = "plot_lib.rs"]
mod plot_lib;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "experiments/out".into());
    match plot_lib::draw_all(std::path::Path::new(&out)) {
        Ok(names) => println!("plotted {} experiments: {}", names.len(), names.join(", ")),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
