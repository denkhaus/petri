//! How a Fabro stage's result becomes an engine outcome: the closed outcome
//! set, the failure class, failure promotion under `on_failure`, and the
//! bookkeeping context keys.
//!
//! Fabro's executor promotes a failed stage under `on_failure="succeed"` only
//! when no explicit route matches the failed outcome, and it checks the
//! routes against the prospective context: the run context with the stage's
//! own updates applied. Petri classifies once, at the step boundary, so the
//! step performs that check here with the node's explicit routes from its
//! config ([`Stage::routes`]). A failure an explicit route matches stays a
//! failure and the routing tiers take that route; any other failure becomes
//! a `PartialSuccess` that keeps the failure in `underlying`, reports
//! `succeeded` (or `partially_succeeded` under Petri's `partially_succeed`
//! extension), and routes as a success. A retryable failure is never
//! promoted here: retries and `allow_partial` exhaustion belong to the
//! engine's retry policy.

use std::collections::BTreeMap;

use frontend_fabro::kinds::{RETRY_REQUESTED_CLASS, StageOutcome};
use frontend_fabro::labels::routing_key;
use frontend_fabro::{Policy, condition};
use ir::{FailureClass, FailureInfo, Outcome, Status, Value};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;

/// The outcome a stage reported, in Fabro's vocabulary: what Fabro's own
/// events show for the stage. A failure `on_failure="succeed"` promoted is a
/// `PartialSuccess` on the record whose reported `outcome` is `succeeded`;
/// every other status maps by [`fabro_outcome`].
pub fn reported_outcome(outcome: &Outcome) -> StageOutcome {
    if matches!(outcome.status, Status::PartialSuccess { .. })
        && outcome.output.get("outcome").and_then(Value::as_str) == Some("succeeded")
    {
        return StageOutcome::Succeeded;
    }
    fabro_outcome(&outcome.status)
}

/// The Fabro spelling of an engine status.
pub fn fabro_outcome(status: &Status) -> StageOutcome {
    match status {
        Status::Success => StageOutcome::Succeeded,
        Status::PartialSuccess { .. } => StageOutcome::PartiallySucceeded,
        Status::Skipped => StageOutcome::Skipped,
        Status::Failure(_) | Status::Cancelled | Status::TimedOut => StageOutcome::Failed,
    }
}

/// The node's explicit routes, as the frontend lowered them
/// (`frontend_fabro::ROUTES_KEY`): what Fabro's edge selection would try
/// before falling through to the failure policy.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ExplicitRoutes {
    /// Every conditional edge's condition text.
    #[serde(default)]
    pub conditions: Vec<String>,
    /// Every unconditional labelled edge's label, in routing-key form.
    #[serde(default)]
    pub labels:     Vec<String>,
    /// Every unconditional edge's target id.
    #[serde(default)]
    pub targets:    Vec<String>,
}

impl ExplicitRoutes {
    /// Whether an explicit route matches a failed outcome: a condition that
    /// holds over the failed status and the prospective context, a preferred
    /// label naming a labelled edge, or a suggested target naming an edge.
    pub fn matches_failure(
        &self,
        output: &serde_json::Map<String, Value>,
        kv: &Value,
        context_updates: &BTreeMap<SmolStr, Value>,
    ) -> bool {
        let label = output
            .get("preferred_label")
            .and_then(Value::as_str)
            .map(routing_key);
        if let Some(label) = &label
            && self.labels.iter().any(|candidate| candidate == label)
        {
            return true;
        }
        let suggested = output
            .get("suggested_next_ids")
            .and_then(Value::as_array)
            .is_some_and(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .any(|id| self.targets.iter().any(|target| target == id))
            });
        if suggested {
            return true;
        }
        if self.conditions.is_empty() {
            return false;
        }
        let mut run = ir::RunContext::new();
        if let Value::Object(map) = kv {
            run.merge(
                &map.iter()
                    .map(|(k, v)| (SmolStr::new(k), v.clone()))
                    .collect(),
            );
        }
        run.merge(context_updates);
        let statics = ir::StaticCtx::new()
            .bind("status", json!("failure"))
            .bind("output", Value::Object(output.clone()));
        let env = ir::EvalEnv::new(&Value::Null, &run, &statics);
        self.conditions.iter().any(|text| {
            let mut table = ir::ExprTable::new();
            let mut diags = frontend::Diagnostics::new();
            let span = frontend::Span::file("condition");
            condition::lower(text, &mut table, &span, &mut diags, false)
                .is_some_and(|expr| ir::eval_bool(&table, expr, &env).unwrap_or(false))
        })
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
    /// The node's explicit routes, from its config, and the run context the
    /// step started with: what promotion checks a failure against.
    pub routes:          Option<ExplicitRoutes>,
    pub kv:              Value,
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
            routes: None,
            kv: Value::Null,
        }
    }

    pub fn failed(reason: impl Into<String>, class: &str, on_failure: Option<Policy>) -> Self {
        let mut stage = Self::new(StageOutcome::Failed, on_failure);
        stage.failure_reason = Some(reason.into());
        stage.failure_class = class.to_string();
        stage
    }

    /// Give the stage what promotion needs: the node's explicit routes and
    /// the run context at spawn. A step whose config carries neither leaves
    /// this alone, and a failure under `succeed` is promoted unconditionally.
    #[must_use]
    pub fn with_routing(mut self, routes: Option<ExplicitRoutes>, kv: Value) -> Self {
        self.routes = routes;
        self.kv = kv;
        self
    }

    /// Whether this failure asks for another attempt.
    pub fn retry_requested(&self) -> bool {
        self.failure_class == RETRY_REQUESTED_CLASS
    }

    /// Whether the policy promotes this failure: it is non-retryable, the
    /// policy says so, and no explicit route matches it.
    fn promotes(&self) -> bool {
        if self.retry_requested()
            || !matches!(
                self.on_failure,
                Some(Policy::Succeed | Policy::PartiallySucceed)
            )
        {
            return false;
        }
        match &self.routes {
            Some(routes) => !routes.matches_failure(&self.output, &self.kv, &self.context_updates),
            None => true,
        }
    }

    /// The engine outcome. A non-retryable failure that no explicit route
    /// matches becomes a `PartialSuccess` here, the one classification
    /// point, with the failure kept in `underlying`. Under `succeed` the
    /// reported `outcome` is `succeeded`, as Fabro reports a promoted stage;
    /// under Petri's `partially_succeed` extension it is
    /// `partially_succeeded`. The event log never records a clean success
    /// for a failed step.
    pub fn into_outcome(mut self, node: &str) -> Outcome {
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
                if self.promotes() {
                    if self.on_failure == Some(Policy::Succeed) {
                        reported = Some(StageOutcome::Succeeded);
                        self.output.insert(
                            "promoted".into(),
                            json!("on_failure=succeed promoted a failed outcome to succeeded"),
                        );
                    }
                    Status::partial(info)
                } else {
                    Status::Failure(info)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn routes() -> ExplicitRoutes {
        ExplicitRoutes {
            conditions: vec!["outcome=failed".into(), "context.mode=fast".into()],
            labels:     vec!["retry".into()],
            targets:    vec!["b".into(), "c".into()],
        }
    }

    #[test]
    fn an_explicit_failure_edge_keeps_the_failure() {
        let stage = Stage::failed("boom", "", Some(Policy::Succeed))
            .with_routing(Some(routes()), Value::Null);
        let outcome = stage.into_outcome("a");
        assert!(matches!(outcome.status, Status::Failure(_)));
        assert_eq!(outcome.output["outcome"], json!("failed"));
    }

    #[test]
    fn an_unmatched_failure_is_promoted_and_keeps_its_evidence() {
        let routes = ExplicitRoutes {
            conditions: vec!["context.mode=fast".into()],
            ..ExplicitRoutes::default()
        };
        let stage = Stage::failed("boom", "exit_status:3", Some(Policy::Succeed))
            .with_routing(Some(routes), json!({ "mode": "slow" }));
        let outcome = stage.into_outcome("a");
        let Status::PartialSuccess { underlying } = &outcome.status else {
            panic!("promoted: {:?}", outcome.status);
        };
        assert_eq!(
            underlying.as_ref().map(|f| f.message.as_str()),
            Some("boom")
        );
        assert_eq!(outcome.output["outcome"], json!("succeeded"));
        assert_eq!(outcome.output["failure_class"], json!("exit_status:3"));
    }

    #[test]
    fn prospective_context_and_labels_count_as_explicit_routes() {
        let mut stage = Stage::failed("boom", "", Some(Policy::Succeed))
            .with_routing(Some(routes()), json!({ "mode": "slow" }));
        stage
            .context_updates
            .insert(SmolStr::new("mode"), json!("fast"));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
        let mut stage = Stage::failed("boom", "", Some(Policy::Succeed)).with_routing(
            Some(ExplicitRoutes {
                labels: vec!["retry".into()],
                ..ExplicitRoutes::default()
            }),
            Value::Null,
        );
        stage
            .output
            .insert("preferred_label".into(), json!("[R] Retry"));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
        let mut stage = Stage::failed("boom", "", Some(Policy::Succeed)).with_routing(
            Some(ExplicitRoutes {
                targets: vec!["c".into()],
                ..ExplicitRoutes::default()
            }),
            Value::Null,
        );
        stage
            .output
            .insert("suggested_next_ids".into(), json!(["c"]));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
    }

    #[test]
    fn a_retryable_failure_is_never_promoted_here() {
        let stage = Stage::failed("flaky", RETRY_REQUESTED_CLASS, Some(Policy::Succeed));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
    }
}
