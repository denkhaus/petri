//! Stub steps: one simulated step per Fabro kind, the way Fabro's `--dry-run`
//! handlers simulate a stage.
//!
//! A stub returns `Success` with `output.outcome = "succeeded"` and
//! `output.simulated = true`; a human gate picks its first choice as the
//! preferred label. A node's config may carry a `simulate` object that scripts
//! the outcome instead — one of the four Fabro outcomes, a failure class, a
//! preferred label, suggested targets, context updates — which is how a test
//! drives every routing tier and failure policy through a real run with no
//! model, shell or person.

use std::collections::BTreeMap;
use std::sync::Arc;

use frontend_fabro::kinds::{
    AGENT_KIND, COMMAND_KIND, HUMAN_KIND, RETRY_REQUESTED_CLASS, WAIT_KIND, WORKFLOW_KIND,
};
use frontend_fabro::labels::strip_accelerator;
use ir::placeholder::contains_placeholder;
use ir::{FailureClass, FailureInfo, Outcome, Status, StepKindId, Value};
use runtime::Runtime;
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Step, StepCtx, StepRunner};

/// What a stub is told to return.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Simulate {
    /// `succeeded` (default), `partially_succeeded`, `failed` or `skipped`.
    #[serde(default)]
    pub outcome:            Option<String>,
    /// The failure class when `outcome` is `failed`: `retry_requested` asks
    /// for another attempt.
    #[serde(default)]
    pub failure_class:      Option<String>,
    #[serde(default)]
    pub failure_reason:     Option<String>,
    #[serde(default)]
    pub preferred_label:    Option<String>,
    #[serde(default)]
    pub suggested_next_ids: Vec<String>,
    #[serde(default)]
    pub context_updates:    BTreeMap<SmolStr, Value>,
    /// Scripted per attempt: the entry at `attempt - 1` wins, the last entry
    /// repeats. Lets one node fail twice and then succeed.
    #[serde(default)]
    pub attempts:           Vec<Self>,
}

#[derive(Deserialize)]
struct StubConfig {
    #[serde(default)]
    node:            Option<String>,
    #[serde(default)]
    on_failure:      Option<String>,
    #[serde(default)]
    choices:         Vec<Choice>,
    #[serde(default)]
    freeform_target: Option<String>,
    #[serde(default)]
    simulate:        Option<Simulate>,
    #[serde(flatten)]
    _rest:           BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct Choice {
    key:   String,
    label: String,
    to:    String,
}

/// The simulated step for one kind.
pub struct StubStep {
    kind: StepKindId,
}

impl StubStep {
    pub fn new(kind: StepKindId) -> Self {
        Self { kind }
    }
}

impl ir::StepKind for StubStep {
    fn id(&self) -> StepKindId {
        self.kind.clone()
    }

    fn name(&self) -> &str {
        self.kind.as_str()
    }

    fn validate_config(&self, config: &Value) -> Result<(), steps::StepFailure> {
        if contains_placeholder(config) {
            return Ok(());
        }
        serde_json::from_value::<StubConfig>(config.clone())
            .map(drop)
            .map_err(|e| steps::StepFailure {
                class:   FailureClass::new_static("bad_config"),
                message: e.to_string(),
            })
    }
}

#[async_trait::async_trait]
impl StepRunner for StubStep {
    async fn run(&self, ctx: StepCtx) -> Outcome {
        let config: StubConfig = match serde_json::from_value(ctx.config.clone()) {
            Ok(config) => config,
            Err(e) => {
                return steps::StepFailure {
                    class:   FailureClass::new_static("bad_config"),
                    message: format!("step config is invalid: {e}"),
                }
                .into();
            }
        };
        let node = config.node.clone().unwrap_or_else(|| ctx.node.to_string());
        let mut script = config.simulate.clone().unwrap_or_default();
        if !script.attempts.is_empty() {
            let index = usize::try_from(ctx.attempt.raw())
                .unwrap_or(1)
                .saturating_sub(1);
            let chosen = script.attempts[index.min(script.attempts.len() - 1)].clone();
            script = chosen;
        }
        let mut output = json!({
            "simulated": true,
            "node": node,
            "text": format!("[Simulated] {}", ctx.node),
        });
        if self.kind == HUMAN_KIND
            && script.preferred_label.is_none()
            && script.suggested_next_ids.is_empty()
        {
            // Fabro's dry run answers a human gate with its first choice.
            if let Some(first) = config.choices.first() {
                script.preferred_label = Some(first.label.clone());
                script.suggested_next_ids = vec![first.to.clone()];
                script
                    .context_updates
                    .entry(SmolStr::new("human.gate.selected"))
                    .or_insert_with(|| json!(first.key));
                script
                    .context_updates
                    .entry(SmolStr::new("human.gate.label"))
                    .or_insert_with(|| json!(first.label));
            } else if let Some(target) = &config.freeform_target {
                script.suggested_next_ids = vec![target.clone()];
            }
        }
        if let Some(label) = &script.preferred_label {
            output["preferred_label"] = json!(strip_accelerator(label));
        }
        if !script.suggested_next_ids.is_empty() {
            output["suggested_next_ids"] = json!(script.suggested_next_ids);
        }
        let outcome_name = script.outcome.clone().unwrap_or_else(|| "succeeded".into());
        let (status, class) = match outcome_name.as_str() {
            "succeeded" => (Status::Success, String::new()),
            "partially_succeeded" => (Status::partial_clean(), String::new()),
            "skipped" => (Status::Skipped, String::new()),
            "failed" => {
                let class = script.failure_class.clone().unwrap_or_default();
                let reason = script
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| format!("[Simulated] {} failed", ctx.node));
                let info = FailureInfo::new(reason).with_class(FailureClass::new(class.as_str()));
                // A non-retryable failure under `on_failure="partially_succeed"`
                // is classified here, at the step boundary, once.
                if config.on_failure.as_deref() == Some("partially_succeed")
                    && class != RETRY_REQUESTED_CLASS
                {
                    (Status::partial(info), class)
                } else {
                    (Status::Failure(info), class)
                }
            }
            other => {
                return steps::StepFailure {
                    class:   FailureClass::new_static("bad_output"),
                    message: format!(
                        "`{other}` is not a stage outcome (succeeded, partially_succeeded, failed, skipped)"
                    ),
                }
                .into();
            }
        };
        output["outcome"] = json!(fabro_outcome(&status));
        output["failure_class"] = json!(class);
        let mut outcome = Outcome::new(status, output);
        outcome.context_updates = script.context_updates;
        outcome
            .context_updates
            .insert(SmolStr::new("failure_class"), json!(class));
        outcome
    }
}

/// The Fabro spelling of an engine status.
pub fn fabro_outcome(status: &Status) -> &'static str {
    match status {
        Status::Success => "succeeded",
        Status::PartialSuccess { .. } => "partially_succeeded",
        Status::Skipped => "skipped",
        Status::Failure(_) | Status::Cancelled | Status::TimedOut => "failed",
    }
}

/// A `Step`-shaped wrapper so a caller can register a stub under a typed
/// config path too.
#[async_trait::async_trait]
impl Step for Simulate {
    const NAME: &'static str = "fabro/simulate";
    type Config = Value;

    async fn run(&self, _config: Value, _ctx: StepCtx) -> Outcome {
        Outcome::success(Value::Null)
    }
}

/// Register a stub for every Fabro step kind.
pub fn register_stubs(runtime: Runtime) -> Runtime {
    let mut registry = runtime.registry().clone();
    for kind in [
        AGENT_KIND,
        COMMAND_KIND,
        HUMAN_KIND,
        WAIT_KIND,
        WORKFLOW_KIND,
    ] {
        registry.register_runner(Arc::new(StubStep::new(kind)));
    }
    runtime.steps(registry)
}
