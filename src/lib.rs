//! Record a container's resource footprint and replay it as synthetic load.
//!
//! The crate is a library plus a thin CLI. `docs/design.md` describes the
//! observables, the measurement tiers and the load generator; each module
//! documents its own contract.

pub mod profile;
