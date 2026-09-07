//! The step kinds a lowered Fabro graph names.
//!
//! The frontend depends on the names, not on the implementations: the real
//! steps and the stub registry both register under these ids, and which
//! registry a run uses is the distribution's choice.

use ir::StepKindId;
use serde::{Deserialize, Serialize};

/// An agent node (`box`): one turn of an agent over ACP or native Pebble.
pub const AGENT_KIND: StepKindId = StepKindId::new_static("fabro/agent");
/// A prompt node (`tab`, or a `tripleoctagon` with a `prompt`): one model
/// call through the application's model client, with no agent tools.
pub const PROMPT_KIND: StepKindId = StepKindId::new_static("fabro/prompt");
/// A command node (`parallelogram`, or any node with a `script`).
pub const COMMAND_KIND: StepKindId = StepKindId::new_static("fabro/command");
/// A human gate (`hexagon`).
pub const HUMAN_KIND: StepKindId = StepKindId::new_static("fabro/human");
/// A wait node (`insulator`).
pub const WAIT_KIND: StepKindId = StepKindId::new_static("fabro/wait");
/// A manager loop (`house`): a nested workflow.
pub const WORKFLOW_KIND: StepKindId = StepKindId::new_static("fabro/workflow");
/// A structural stage (`start`, `exit`): runs nothing, records where its scope
/// runs so hooks placed in the sandbox find it, and fires the run-level hooks.
pub const STAGE_KIND: StepKindId = StepKindId::new_static("fabro/stage");
/// Reserved name of the synthetic node that enforces goal gates.
pub const GOAL_CHECK_NODE: &str = "goal_check";
/// The most repair turns one agent step accepts from configuration.
pub const MAX_OUTPUT_RETRIES: u64 = 100;

/// Every kind the frontend emits, for a registry that stubs them all.
pub const ALL: &[&StepKindId] = &[
    &AGENT_KIND,
    &PROMPT_KIND,
    &COMMAND_KIND,
    &HUMAN_KIND,
    &WAIT_KIND,
    &WORKFLOW_KIND,
    &STAGE_KIND,
];

/// The closed set of stage outcomes a Fabro step may report, and the only
/// values `outcome=X` may name in a condition.
pub const OUTCOMES: &[&str] = &["succeeded", "partially_succeeded", "failed", "skipped"];

/// The closed set of outcomes a Fabro stage reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageOutcome {
    #[default]
    Succeeded,
    PartiallySucceeded,
    Failed,
    Skipped,
}

impl StageOutcome {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "succeeded" => Self::Succeeded,
            "partially_succeeded" => Self::PartiallySucceeded,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::PartiallySucceeded => "partially_succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// The failure class a step reports when it wants another attempt: the one
/// class the lowered retry policy retries on.
pub const RETRY_REQUESTED_CLASS: &str = "retry_requested";

/// The day the `outcome=success` condition alias expires. The alias names
/// this date in its warning and carries a `REMOVE AFTER 2026-10-04` comment.
/// `on_failure="succeed"` and `auto_status` are no longer shims: Fabro's
/// reference revision supports both, so Petri keeps them for as long as the
/// reference does.
///
/// REMOVE AFTER 2026-10-04: delete the alias and this constant together.
pub const COMPAT_SUNSET: &str = "2026-10-04";
