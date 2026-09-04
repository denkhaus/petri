//! How a Fabro stage's result becomes an engine outcome: the closed outcome
//! set, the failure class, the `on_failure="partially_succeed"` conversion
//! at the step boundary, and the bookkeeping context keys.

use std::collections::BTreeMap;

use frontend_fabro::Policy;
use frontend_fabro::kinds::{RETRY_REQUESTED_CLASS, StageOutcome};
use ir::{FailureClass, FailureInfo, Outcome, Status, Value};
use serde_json::json;
use smol_str::SmolStr;

/// The Fabro spelling of an engine status.
pub fn fabro_outcome(status: &Status) -> StageOutcome {
    match status {
        Status::Success => StageOutcome::Succeeded,
        Status::PartialSuccess { .. } => StageOutcome::PartiallySucceeded,
        Status::Skipped => StageOutcome::Skipped,
        Status::Failure(_) | Status::Cancelled | Status::TimedOut => StageOutcome::Failed,
    }
}

/// What a stage reports, before it becomes an outcome.
pub struct Stage {
    pub outcome:         StageOutcome,
    pub failure_reason:  Option<String>,
    pub failure_class:   String,
    pub output:          serde_json::Map<String, Value>,
    pub context_updates: BTreeMap<SmolStr, Value>,
    /// The node's `on_failure` policy, from its config.
    pub on_failure:      Option<Policy>,
}

impl Stage {
    pub fn new(outcome: StageOutcome, on_failure: Option<Policy>) -> Self {
        Self {
            outcome,
            failure_reason: None,
            failure_class: String::new(),
            output: serde_json::Map::new(),
            context_updates: BTreeMap::new(),
            on_failure,
        }
    }

    pub fn failed(reason: impl Into<String>, class: &str, on_failure: Option<Policy>) -> Self {
        let mut stage = Self::new(StageOutcome::Failed, on_failure);
        stage.failure_reason = Some(reason.into());
        stage.failure_class = class.to_string();
        stage
    }

    /// Whether this failure asks for another attempt.
    pub fn retry_requested(&self) -> bool {
        self.failure_class == RETRY_REQUESTED_CLASS
    }

    /// The engine outcome. A non-retryable failure under
    /// `on_failure="partially_succeed"` becomes a `PartialSuccess` here — the
    /// one classification point — with the failure kept in `underlying`.
    /// Under the `succeed` shim it is the same partial status, but the
    /// reported `outcome` is `succeeded`, as Fabro reports it.
    pub fn into_outcome(mut self, node: &str) -> Outcome {
        // REMOVE AFTER 2026-10-04: drop the `succeed` arm and `reported`.
        let mut reported = None;
        let status = match self.outcome {
            StageOutcome::Succeeded => Status::Success,
            StageOutcome::PartiallySucceeded => Status::partial_clean(),
            StageOutcome::Skipped => Status::Skipped,
            StageOutcome::Failed => {
                let reason = self
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| format!("stage `{node}` failed"));
                let info = FailureInfo::new(reason)
                    .with_class(FailureClass::new(self.failure_class.as_str()));
                match self.on_failure {
                    Some(Policy::PartiallySucceed) if !self.retry_requested() => {
                        Status::partial(info)
                    }
                    Some(Policy::Succeed) if !self.retry_requested() => {
                        reported = Some(StageOutcome::Succeeded);
                        Status::partial(info)
                    }
                    _ => Status::Failure(info),
                }
            }
        };
        let reported = reported.unwrap_or_else(|| fabro_outcome(&status));
        self.output
            .insert("outcome".into(), json!(reported.as_str()));
        self.output
            .insert("failure_class".into(), json!(self.failure_class));
        if let Some(reason) = &self.failure_reason {
            self.output.insert("failure_reason".into(), json!(reason));
        }
        let mut outcome = Outcome::new(status, Value::Object(self.output));
        outcome.context_updates = self.context_updates;
        outcome
            .context_updates
            .insert(SmolStr::new("failure_class"), json!(self.failure_class));
        outcome
    }
}
