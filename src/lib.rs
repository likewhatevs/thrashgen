//! Record a container's resource footprint and replay it as synthetic load.
//!
//! The crate is a library plus a thin CLI. `docs/design.md` describes the
//! observables, the measurement tiers and the load generator; each module
//! documents its own contract.

// Pedantic lints are on; the groups below are allowed with a reason,
// since the alternative, local allows at hundreds of sites, would hide
// the signal.
#![warn(clippy::pedantic)]
#![allow(
    // Counts become f64 for ratios and come back as knob values; the
    // precision and sign of a measurement tool's arithmetic are its
    // own concern, checked by its tests.
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    // Exact comparisons are deliberate: a knob unchanged, a rate of zero.
    clippy::float_cmp,
    // Line-aligned addresses are cast to word pointers; the alignment is
    // the stream's invariant, not the type's.
    clippy::cast_ptr_alignment,
    // t, v, d, k, m, g: the fitter's notation, defined once in the design.
    clippy::similar_names,
    clippy::many_single_char_names,
    // Documentation names what a function is for, not every error path.
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::struct_excessive_bools,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    // The access path is inlined by design and the choice was measured.
    clippy::inline_always
)]

pub mod aggregate;
pub mod calibration;
pub mod caps;
pub mod cgroup;
pub mod cli;
pub mod compare;
pub mod cpu;
pub mod events;
pub mod fit;
pub mod footprint;
pub mod loadgen;
pub mod measure;
pub mod perf;
pub mod plan;
pub mod profile;
pub mod record;
pub mod report;
pub mod resctrl;
pub mod run;
pub mod sampler;
pub mod samples;
pub mod sys;
pub mod topology;
