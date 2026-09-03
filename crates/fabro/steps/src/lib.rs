//! Fabro step kinds: what a lowered Fabro graph runs.
//!
//! The frontend names five kinds (`frontend_fabro::kinds`); this crate
//! registers them. [`register`] installs the real steps as they land, and
//! [`register_stubs`] installs one simulated step per name — Fabro's
//! `--dry-run` handlers — so a graph lowers, validates and runs end to end
//! before any real step exists, and so `petri run --dry-run` stays possible
//! afterwards. Which registry a run uses is the distribution's choice.

mod stub;

use runtime::Runtime;
pub use stub::{Simulate, StubStep, fabro_outcome, register_stubs};

/// Register the real Fabro step kinds on a runtime. Kinds without a real
/// implementation yet register their stub, so every lowered graph validates.
pub fn register(runtime: Runtime) -> Runtime {
    register_stubs(runtime)
}
