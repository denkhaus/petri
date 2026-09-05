//! `fabro/workflow`: Fabro's manager loop as a nested invocation.
//!
//! The child graph was lowered with the parent and registered before the run
//! started; the step names it by digest and runs it through the coordinator's
//! `InvocationClient`, once per cycle, until the `manager.stop_condition`
//! holds over the child's final context or `manager.max_cycles` is reached.
//! Each cycle is its own call site, so a re-dispatched attempt attaches to
//! the invocation it already declared. The child inherits the parent's
//! sandbox and secrets; the parent's own cancellation cancels it.

use std::collections::BTreeMap;
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
    1
}

pub struct WorkflowStep;

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

/// Whether the stop condition holds over a finished child: Fabro's condition
/// grammar over the child's final context, with `outcome` as the child's
/// status.
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

    fn holds(&self, status: RunStatus, context: &BTreeMap<SmolStr, Value>) -> bool {
        let mut run = ir::RunContext::new();
        run.merge(context);
        let tag = match status {
            RunStatus::Success => "success",
            RunStatus::Failed => "failure",
            RunStatus::Cancelled => "cancelled",
        };
        let statics = ir::StaticCtx::new()
            .bind("status", json!(tag))
            .bind("output", Value::Null);
        ir::eval_bool(
            &self.table,
            self.expr,
            &ir::EvalEnv::new(&Value::Null, &run, &statics),
        )
        .unwrap_or(false)
    }
}

async fn wait_interval(duration: Duration, control: &mut mpsc::Receiver<Control>) -> bool {
    let sleep = time::sleep(duration);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return true,
            ctl = control.recv() => match ctl {
                Some(Control::Deliver(_)) => {}
                _ => return false,
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
        let client = match ctx.require_capability::<CoordinatorInvocationClient>() {
            Ok(client) => client,
            Err(failure) => return failure.into(),
        };
        let stop_condition = match config
            .stop_condition
            .as_deref()
            .map(StopCondition::compile)
            .transpose()
        {
            Ok(condition) => condition,
            Err(error) => return fail(error, "bad_config"),
        };
        let mut context = public_context(&config.kv);
        let before = context.clone();
        let mut last_status = RunStatus::Success;
        let mut last_output = Value::Null;
        let mut cycles = 0_u64;
        let max_cycles = config.max_cycles.max(1);
        while cycles < max_cycles {
            cycles += 1;
            let request = InvocationRequest {
                site:    CallSite {
                    firing:  ctx.firing,
                    attempt: ctx.attempt,
                    slot:    SmolStr::new(format!("{}/{cycles}", config.node)),
                },
                graph:   config.child_digest,
                context: context.clone(),
                secrets: SecretBindings::Inherit,
                sandbox: SandboxMode::Inherit { scope: ctx.scope },
            };
            let mut handle = match client.start_or_attach(request).await {
                Ok(handle) => handle,
                Err(error) => return fail(error.to_string(), "invocation"),
            };
            let Some(result) = handle.result_with_control(&mut ctx.control).await else {
                return Outcome::cancelled();
            };
            last_status = result.status;
            last_output = result.output.clone();
            for (key, value) in &result.context {
                context.insert(key.clone(), value.clone());
            }
            if result.status != RunStatus::Success {
                break;
            }
            if let Some(condition) = &stop_condition
                && condition.holds(result.status, &result.context)
            {
                break;
            }
            if cycles < max_cycles
                && let Some(ms) = config.poll_interval_ms.filter(|ms| *ms > 0)
                && !wait_interval(Duration::from_millis(ms), &mut ctx.control).await
            {
                return Outcome::cancelled();
            }
        }
        let mut stage = match last_status {
            RunStatus::Success => Stage::new(StageOutcome::Succeeded, config.on_failure),
            RunStatus::Failed => Stage::failed(
                format!("the nested workflow `{}` failed", config.node),
                "child_failed",
                config.on_failure,
            ),
            RunStatus::Cancelled => return Outcome::cancelled(),
        };
        stage.output.insert("cycles".into(), json!(cycles));
        stage.output.insert("child".into(), last_output);
        // The parent sees what the child changed, and nothing it left alone.
        for (key, value) in &context {
            if before.get(key) != Some(value) {
                stage.context_updates.insert(key.clone(), value.clone());
            }
        }
        stage.into_outcome(&config.node)
    }
}
