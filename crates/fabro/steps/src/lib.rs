//! Fabro step kinds: what a lowered Fabro graph runs.
//!
//! The frontend names five kinds (`frontend_fabro::kinds`); this crate
//! registers them. [`register`] installs the real steps — `fabro/command`,
//! `fabro/wait`, `fabro/human`, `fabro/agent` over ACP and `fabro/workflow` —
//! and [`register_stubs`] installs one simulated step per name (Fabro's
//! `--dry-run` handlers) so a graph lowers, validates and runs end to end with
//! no model, shell or person. Which registry a run uses is the distribution's
//! choice.

pub mod acp;
pub mod agent;
pub mod command;
pub mod directive;
pub mod human;
mod outcome;
pub mod pebble;
mod stub;
pub mod wait;
pub mod workflow;

pub use agent::AgentStep;
pub use command::CommandStep;
pub use frontend_fabro::kinds::{AGENT_KIND, COMMAND_KIND, HUMAN_KIND, WAIT_KIND, WORKFLOW_KIND};
pub use human::HumanStep;
pub use outcome::{Stage, fabro_outcome};
use runtime::Runtime;
pub use stub::{Simulate, StubStep, register_stubs};
pub use wait::WaitStep;
pub use workflow::WorkflowStep;

/// Register the real Fabro step kinds on a runtime. Kinds without a real
/// implementation yet register their stub, so every lowered graph validates.
pub fn register(runtime: Runtime) -> Runtime {
    runtime
        .step(CommandStep)
        .step(WaitStep)
        .step(HumanStep)
        .step(AgentStep)
        .step(WorkflowStep)
}
