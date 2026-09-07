//! Fabro step kinds: what a lowered Fabro graph runs.
//!
//! The frontend names six kinds (`frontend_fabro::kinds`); this crate
//! registers them. [`register`] installs the real steps — `fabro/command`,
//! `fabro/wait`, `fabro/human`, `fabro/agent` over ACP or native Pebble,
//! `fabro/prompt` and `fabro/workflow` — and [`register_stubs`] installs one
//! simulated step per name (Fabro's `--dry-run` handlers) so a graph lowers,
//! validates and runs end to end with no model, shell or person. Which
//! registry a run uses is the distribution's choice.
//!
//! [`register`] also installs the run's output-reference store
//! ([`blobs::OutputStore`], a [`blobs::LocalBlobStore`] under
//! `<run_dir>/blobs`) unless the host registered its own before the run.

pub mod acp;
pub mod agent;
pub mod blobs;
pub mod command;
pub mod contract;
pub mod directive;
pub mod human;
mod outcome;
pub mod pebble;
pub mod preamble;
pub mod prompt;
mod stub;
pub mod wait;
pub mod workflow;

use std::sync::Arc;

pub use agent::AgentStep;
pub use blobs::{BlobStore, LocalBlobStore, OutputStore};
pub use command::CommandStep;
pub use frontend_fabro::kinds::{
    AGENT_KIND, COMMAND_KIND, HUMAN_KIND, PROMPT_KIND, WAIT_KIND, WORKFLOW_KIND,
};
pub use human::HumanStep;
pub use outcome::{ExplicitRoutes, Stage, fabro_outcome};
pub use prompt::PromptStep;
use runtime::Runtime;
pub use stub::{Simulate, StubStep, register_stubs};
pub use wait::WaitStep;
pub use workflow::WorkflowStep;

/// The run-dir subdirectory the default output store writes under.
pub const BLOBS_DIR: &str = "blobs";

/// Register the real Fabro step kinds on a runtime, plus the default
/// output-reference store for runs whose host supplied none.
pub fn register(runtime: Runtime) -> Runtime {
    runtime
        .step(CommandStep)
        .step(WaitStep)
        .step(HumanStep)
        .step(AgentStep)
        .step(PromptStep)
        .step(WorkflowStep)
        .run_services(|run_dir, caps| {
            if caps.has::<OutputStore>() {
                return (caps, None);
            }
            let store = LocalBlobStore::new(run_dir.join(BLOBS_DIR));
            (caps.provide(OutputStore(Arc::new(store))), None)
        })
}
