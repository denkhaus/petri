//! The step kinds a lowered Fabro graph names.
//!
//! The frontend depends on the names, not on the implementations: the real
//! steps and the stub registry both register under these ids, and which
//! registry a run uses is the distribution's choice.

use ir::StepKindId;

/// An agent or prompt node (`box`, `tab`): one turn of an agent over ACP.
pub const AGENT_KIND: StepKindId = StepKindId::new_static("fabro/agent");
/// A command node (`parallelogram`, or any node with a `script`).
pub const COMMAND_KIND: StepKindId = StepKindId::new_static("fabro/command");
/// A human gate (`hexagon`).
pub const HUMAN_KIND: StepKindId = StepKindId::new_static("fabro/human");
/// A wait node (`insulator`).
pub const WAIT_KIND: StepKindId = StepKindId::new_static("fabro/wait");
/// A manager loop (`house`): a nested workflow.
pub const WORKFLOW_KIND: StepKindId = StepKindId::new_static("fabro/workflow");

/// Every kind the frontend emits, for a registry that stubs them all.
pub const ALL: &[&StepKindId] = &[
    &AGENT_KIND,
    &COMMAND_KIND,
    &HUMAN_KIND,
    &WAIT_KIND,
    &WORKFLOW_KIND,
];

/// The closed set of stage outcomes a Fabro step may report, and the only
/// values `outcome=X` may name in a condition.
pub const OUTCOMES: &[&str] = &["succeeded", "partially_succeeded", "failed", "skipped"];

/// The failure class a step reports when it wants another attempt: the one
/// class the lowered retry policy retries on.
pub const RETRY_REQUESTED_CLASS: &str = "retry_requested";
