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
pub mod fallback;
pub mod fidelity;
pub mod hooks;
pub mod human;
pub mod memory;
mod outcome;
pub mod parallel;
pub mod pebble;
pub mod preamble;
pub mod prompt;
pub mod sessions;
pub mod skills;
pub mod stage;
mod stub;
pub mod wait;
pub mod workflow;

use std::sync::Arc;

pub use agent::AgentStep;
pub use blobs::{BlobStore, LocalBlobStore, OutputStore};
pub use command::CommandStep;
use execution::hooks::{HookAdapter, HookService, HookServiceHandle};
pub use frontend_fabro::kinds::{
    AGENT_KIND, BRANCH_KIND, COMMAND_KIND, FAN_IN_KIND, HUMAN_KIND, PROMPT_KIND, STAGE_KIND,
    WAIT_KIND, WORKFLOW_KIND,
};
pub use human::HumanStep;
pub use outcome::{ExplicitRoutes, Stage, fabro_outcome, reported_outcome};
pub use parallel::{BranchStep, FanInStep};
pub use prompt::PromptStep;
use runtime::Runtime;
pub use stage::StageStep;
pub use stub::{Simulate, StubScripts, StubStep, register_stubs};
pub use wait::WaitStep;
pub use workflow::WorkflowStep;

/// The run-dir subdirectory the default output store writes under.
pub const BLOBS_DIR: &str = "blobs";

/// Register the real Fabro step kinds on a runtime, plus the run services
/// Fabro workflows need: the default output-reference store, the local hook
/// service (installed as the driver's awaited hooks and as the
/// `HookServiceHandle` capability, unless the host installed its own), the
/// retained-session service, the model fallback service, and the run
/// identity hooks read.
///
/// A host that supplies its own `HookService` registers a `HookServiceHandle`
/// capability and its own `Runtime::hooks` before calling this; the local
/// service then steps aside, so no hook runs twice.
pub fn register(runtime: Runtime) -> Runtime {
    services(
        runtime
            .step(CommandStep)
            .step(WaitStep)
            .step(HumanStep)
            .step(AgentStep)
            .step(PromptStep)
            .step(WorkflowStep)
            .step(StageStep)
            .step(BranchStep)
            .step(FanInStep),
    )
}

/// The run services alone, for a host or a test that registers its own mix
/// of real and simulated Fabro steps: the local hook service, the output
/// store, the retained sessions, and the run identity.
pub fn services(runtime: Runtime) -> Runtime {
    let runtime = if runtime.installed_hooks().is_some() {
        runtime
    } else {
        let local = Arc::new(hooks::LocalHooks::default());
        let service: Arc<dyn HookService> = local.clone();
        runtime
            .hooks(Arc::new(HookAdapter::new(service.clone())))
            .capability(HookServiceHandle(service))
            .capability(LocalHooksHandle(local))
    };
    runtime.run_services(|run_dir, caps| {
        let mut caps = caps;
        if !caps.has::<OutputStore>() {
            let store = LocalBlobStore::new(run_dir.join(BLOBS_DIR));
            caps = caps.provide(OutputStore(Arc::new(store)));
        }
        let run = stage::RunInfo {
            run_id: run_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        if let Some(local) = caps.get::<LocalHooksHandle>() {
            local.0.set_run(run.clone());
            local
                .0
                .set_client(caps.get::<pebble::PebbleClient>().map(|c| (*c).clone()));
        }
        (
            caps.provide(run)
                .provide(sessions::SessionService::new())
                .provide(fallback::FallbackService::new()),
            None,
        )
    })
}

/// The local hook service, registered so steps that drive their own points
/// (`fabro/stage`, the ACP client) can reach its configuration.
pub struct LocalHooksHandle(pub Arc<hooks::LocalHooks>);
