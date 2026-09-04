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

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

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

use crate::outcome::fabro_outcome;

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
    /// Scripted per call: the n-th time this node's stub runs in a run — an
    /// attempt after a retry, a second visit after a goal-gate jump or a
    /// `loop_restart` — takes the n-th entry, and the last entry repeats.
    /// Fabro's engine calls a handler once per attempt in the same order, so
    /// the oracle generator scripts its handlers the same way.
    #[serde(default)]
    pub calls:              Vec<Self>,
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
        if !script.calls.is_empty() {
            let call = next_call(ctx.env.workspace_path(), &ctx.node);
            let chosen = script.calls[call.min(script.calls.len() - 1)].clone();
            script = chosen;
        }
        let mut output = json!({
            "simulated": true,
            "node": node,
            "text": format!("[Simulated] {}", ctx.node),
        });
        let answers = script.outcome.as_deref().is_none_or(|o| o == "succeeded");
        if self.kind == HUMAN_KIND
            && answers
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

/// How many times a scripted stub has run for `node` in the run whose
/// workspace is `workspace`, counted here because a step has no memory of
/// its own across attempts, visits and restarted executions. The workspace
/// path is one per invocation, so a run's successor executions share the
/// count while different runs (different run dirs) do not. Test-only state:
/// replay never runs a step, so the counter cannot touch determinism.
fn next_call(workspace: &str, node: &str) -> usize {
    static CALLS: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);
    let mut calls = CALLS.lock().expect("the stub call table is not poisoned");
    let table = calls.get_or_insert_with(HashMap::new);
    let count = table.entry(format!("{workspace}/{node}")).or_insert(0);
    let current = *count;
    *count += 1;
    current
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
