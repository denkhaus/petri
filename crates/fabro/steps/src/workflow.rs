//! `fabro/workflow`: Fabro's manager loop as a nested invocation.
//!
//! The child graph was lowered with the parent and registered before the run
//! started. The step starts the child once per manager attempt through the
//! coordinator's `InvocationClient` at one durable call site, so a
//! re-dispatched attempt after a crash reattaches to the child it already
//! declared instead of starting another. It then polls, as Fabro's
//! `SubWorkflowHandler` does: every `manager.poll_interval` (45 seconds by
//! default) it evaluates `manager.stop_condition` against the parent's
//! context with a reference success outcome; a satisfied condition cancels
//! the child and returns success; `manager.max_cycles` polls without child
//! completion cancels the child and fails. A child that completes first
//! returns its status, its failure and its filtered context updates. The
//! child inherits the parent's sandbox and secrets; the parent's own
//! cancellation cancels it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use execution::{
    CallSite, CoordinatorInvocationClient, GraphDigest, InvocationClient, InvocationRequest,
    SandboxMode, SecretBindings,
};
use frontend_fabro::kinds::{StageOutcome, WORKFLOW_KIND};
use frontend_fabro::{Policy, condition};
use ir::{Control, Outcome, RunStatus, StepKindId, Value};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Step, StepCtx};
use tokio::sync::mpsc;
use tokio::time;

use crate::outcome::Stage;

pub const KIND: StepKindId = WORKFLOW_KIND;

/// Fabro's default poll interval.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 45_000;

/// Fabro's `manager.max_cycles` when the attribute is missing or not a
/// non-negative integer.
pub const DEFAULT_MAX_CYCLES: u64 = 1000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfig {
    pub label:            String,
    pub node:             String,
    #[serde(default)]
    pub child_workflow:   Option<String>,
    #[serde(default)]
    pub child_dot_source: Option<String>,
    /// The registered child graph, by digest.
    pub child_digest:     GraphDigest,
    #[serde(default = "default_max_cycles")]
    pub max_cycles:       u64,
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
    #[serde(default)]
    pub stop_condition:   Option<String>,
    #[serde(default)]
    pub on_failure:       Option<Policy>,
    #[serde(default)]
    pub kv:               Value,
}

fn default_max_cycles() -> u64 {
    DEFAULT_MAX_CYCLES
}

pub struct WorkflowStep;

/// A host-supplied invocation client for child workflows. The coordinator
/// registers its own client on every run; a host that runs Fabro steps
/// outside the coordinator, or a test with a controlled clock, registers
/// this instead and the step uses it first.
#[derive(Clone)]
pub struct ChildInvoker(pub Arc<dyn InvocationClient>);

/// The keys of the parent's context a child receives: everything a stage
/// wrote that is not bookkeeping.
fn public_context(kv: &Value) -> BTreeMap<SmolStr, Value> {
    let Value::Object(map) = kv else {
        return BTreeMap::new();
    };
    map.iter()
        .filter(|(k, _)| {
            !(k.starts_with("internal.")
                || k.starts_with("response.")
                || matches!(k.as_str(), "failure_class" | "last_response" | "last_stage"))
        })
        .map(|(k, v)| (SmolStr::new(k), v.clone()))
        .collect()
}

/// Fabro's engine-internal keys: a child's change to one never reaches the
/// parent.
fn is_engine_internal(key: &str) -> bool {
    key.starts_with("internal.")
        || key.starts_with("graph.")
        || key.starts_with("thread.")
        || key.starts_with("current")
}

/// The stop condition: Fabro's condition grammar over the parent's context.
struct StopCondition {
    table: ir::ExprTable,
    expr:  ir::ExprId,
}

impl StopCondition {
    fn compile(condition: &str) -> Result<Self, String> {
        let mut table = ir::ExprTable::new();
        let mut diags = frontend::Diagnostics::new();
        let span = frontend::Span::file("manager.stop_condition");
        let Some(expr) = condition::lower(condition, &mut table, &span, &mut diags, false) else {
            return Err(diags
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "));
        };
        Ok(Self { table, expr })
    }

    /// Whether the condition holds over `context` with Fabro's reference
    /// success outcome: `outcome=succeeded`, no preferred label.
    fn holds(&self, context: &BTreeMap<SmolStr, Value>) -> bool {
        let mut run = ir::RunContext::new();
        run.merge(context);
        let statics = ir::StaticCtx::new()
            .bind("status", json!("success"))
            .bind("output", json!({ "outcome": "succeeded" }));
        ir::eval_bool(
            &self.table,
            self.expr,
            &ir::EvalEnv::new(&Value::Null, &run, &statics),
        )
        .unwrap_or(false)
    }
}

/// What ended one poll interval.
enum Poll {
    /// The interval elapsed with no child result.
    Elapsed,
    /// The child finished.
    Finished(execution::InvocationResult),
    /// The parent was cancelled or killed.
    Cancelled,
}

async fn poll(
    handle: &mut execution::InvocationHandle,
    interval: Duration,
    control: &mut mpsc::Receiver<Control>,
) -> Poll {
    let sleep = time::sleep(interval);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            result = handle.result() => return Poll::Finished(result),
            () = &mut sleep => return Poll::Elapsed,
            ctl = control.recv() => {
                if !matches!(ctl, Some(Control::Deliver(_))) {
                    handle.cancel().await;
                    return Poll::Cancelled;
                }
            },
        }
    }
}

#[async_trait::async_trait]
impl Step for WorkflowStep {
    const NAME: &'static str = "fabro/workflow";
    type Config = WorkflowConfig;

    async fn run(&self, config: WorkflowConfig, mut ctx: StepCtx) -> Outcome {
        let fail = |reason: String, class: &str| {
            Stage::failed(reason, class, config.on_failure).into_outcome(&config.node)
        };
        let client: Arc<dyn InvocationClient> = match ctx.capability::<ChildInvoker>() {
            Some(invoker) => invoker.0.clone(),
            None => match ctx.require_capability::<CoordinatorInvocationClient>() {
                Ok(client) => client,
                Err(failure) => return failure.into(),
            },
        };
        let stop_condition = match config
            .stop_condition
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .map(StopCondition::compile)
            .transpose()
        {
            Ok(condition) => condition,
            Err(error) => return fail(error, "bad_config"),
        };
        let max_cycles = config.max_cycles.max(1);
        let interval = Duration::from_millis(
            config
                .poll_interval_ms
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_POLL_INTERVAL_MS),
        );
        let parent_context = public_context(&config.kv);
        // One durable call site per manager attempt: the slot names the node
        // and nothing else, so a re-dispatch of this attempt reattaches to the
        // child it declared, and a later attempt starts a fresh child.
        let request = InvocationRequest {
            site:    CallSite {
                firing:  ctx.firing,
                attempt: ctx.attempt,
                slot:    SmolStr::new(config.node.as_str()),
            },
            graph:   config.child_digest,
            context: parent_context.clone(),
            secrets: SecretBindings::Inherit,
            sandbox: SandboxMode::Inherit { scope: ctx.scope },
        };
        let mut handle = match client.start_or_attach(request).await {
            Ok(handle) => handle,
            Err(error) => return fail(error.to_string(), "invocation"),
        };
        let mut cycles = 0_u64;
        while cycles < max_cycles {
            cycles += 1;
            match poll(&mut handle, interval, &mut ctx.control).await {
                Poll::Cancelled => return Outcome::cancelled(),
                Poll::Finished(result) => {
                    return child_completed(&config, &parent_context, result, cycles);
                }
                Poll::Elapsed => {
                    if let Some(condition) = &stop_condition
                        && condition.holds(&parent_context)
                    {
                        handle.cancel().await;
                        let mut stage = Stage::new(StageOutcome::Succeeded, config.on_failure);
                        stage.output.insert("cycles".into(), json!(cycles));
                        stage.output.insert(
                            "notes".into(),
                            json!(format!("Stop condition satisfied at cycle {cycles}")),
                        );
                        stage
                            .output
                            .insert("child".into(), json!(handle.id().raw()));
                        return stage.into_outcome(&config.node);
                    }
                }
            }
        }
        handle.cancel().await;
        let mut stage = Stage::failed(
            format!(
                "Max cycles ({max_cycles}) exceeded for manager loop node: {}",
                config.node
            ),
            "max_cycles",
            config.on_failure,
        );
        stage.output.insert("cycles".into(), json!(cycles));
        stage
            .output
            .insert("child".into(), json!(handle.id().raw()));
        stage.into_outcome(&config.node)
    }
}

/// The child finished: its status, its failure when it failed, and the
/// public keys it changed, as Fabro's `context_diff_public` reports them.
fn child_completed(
    config: &WorkflowConfig,
    before: &BTreeMap<SmolStr, Value>,
    result: execution::InvocationResult,
    cycles: u64,
) -> Outcome {
    let mut stage = match result.status {
        RunStatus::Success => Stage::new(StageOutcome::Succeeded, config.on_failure),
        RunStatus::Failed => {
            let reason = result.failure.as_ref().map_or_else(
                || format!("the nested workflow `{}` failed", config.node),
                |failure| failure.message.clone(),
            );
            let class = result
                .failure
                .as_ref()
                .map(|failure| failure.class.as_str().to_string())
                .filter(|class| !class.is_empty())
                .unwrap_or_else(|| "child_failed".to_string());
            Stage::failed(reason, &class, config.on_failure)
        }
        RunStatus::Cancelled => return Outcome::cancelled(),
    };
    stage.output.insert("cycles".into(), json!(cycles));
    stage.output.insert(
        "notes".into(),
        json!(format!("Child completed at cycle {cycles}")),
    );
    stage
        .output
        .insert("child".into(), json!(result.final_execution.raw()));
    stage.output.insert("child_output".into(), result.output);
    for (key, value) in &result.context {
        if is_engine_internal(key) || before.get(key) == Some(value) {
            continue;
        }
        stage.context_updates.insert(key.clone(), value.clone());
    }
    stage.into_outcome(&config.node)
}
