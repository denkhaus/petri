//! Fabro step kinds: what a lowered Fabro graph runs.
//!
//! The frontend names five kinds (`frontend_fabro::kinds`); this crate
//! registers them. [`register`] installs the real steps — `fabro/command`,
//! `fabro/wait`, `fabro/human`, and, as they land, `fabro/agent` and
//! `fabro/workflow` — and [`register_stubs`] installs one simulated step per
//! name (Fabro's `--dry-run` handlers) so a graph lowers, validates and runs
//! end to end with no model, shell or person. Which registry a run uses is the
//! distribution's choice.

pub mod command;
pub mod directive;
pub mod human;
mod outcome;
mod stub;
pub mod wait;

use std::sync::Arc;

pub use command::CommandStep;
pub use frontend_fabro::kinds::{AGENT_KIND, COMMAND_KIND, HUMAN_KIND, WAIT_KIND, WORKFLOW_KIND};
pub use human::HumanStep;
pub use outcome::{Stage, fabro_outcome};
use runtime::Runtime;
pub use stub::{Simulate, StubStep, register_stubs};
pub use wait::WaitStep;

/// Register the real Fabro step kinds on a runtime. Kinds without a real
/// implementation yet register their stub, so every lowered graph validates.
pub fn register(runtime: Runtime) -> Runtime {
    let runtime = runtime.step(CommandStep).step(WaitStep).step(HumanStep);
    let mut registry = runtime.registry().clone();
    for kind in [AGENT_KIND, WORKFLOW_KIND] {
        registry.register_runner(Arc::new(StubStep::new(kind)));
    }
    runtime.steps(registry)
}
