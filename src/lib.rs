//! Record a container's resource footprint and replay it as synthetic load.
//!
//! The crate is a library plus a thin CLI. `docs/design.md` describes the
//! observables, the measurement tiers and the load generator; each module
//! documents its own contract.

pub mod aggregate;
pub mod caps;
pub mod cgroup;
pub mod cli;
pub mod cpu;
pub mod events;
pub mod footprint;
pub mod measure;
pub mod perf;
pub mod plan;
pub mod profile;
pub mod record;
pub mod resctrl;
pub mod sampler;
pub mod samples;
pub mod sys;
pub mod topology;
