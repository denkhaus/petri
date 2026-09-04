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
use std::str::FromStr;

use execution::{
    CallSite, CoordinatorInvocationClient, GraphDigest, InvocationClient, InvocationRequest,
    SandboxMode, SecretBindings,
};
use frontend_fabro::condition;
use frontend_fabro::kinds::WORKFLOW_KIND;
use ir::{Control, Outcome, RunStatus, StepKindId, Value};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Step, StepCtx};

use crate::outcome::Stage;

pub const KIND: StepKindId = WORKFLOW_KIND;

#[derive(Debug, Deserialize)]
pub struct WorkflowConfig {
    pub label:            String,
    pub node:             String,
    #[serde(default)]
    pub child_workflow:   Option<String>,
    #[serde(default)]
    pub child_dot_source: Option<String>,
    /// The registered child graph, by digest.
    pub child_digest:     String,
    #[serde(default = "default_max_cycles")]
    pub max_cycles:       u64,
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
    #[serde(default)]
    pub stop_condition:   Option<String>,
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
fn stop_holds(condition: &str, status: RunStatus, context: &BTreeMap<SmolStr, Value>) -> bool {
    let mut table = ir::ExprTable::new();
    let mut diags = frontend::Diagnostics::new();
    let span = frontend::Span::file("manager.stop_condition");
    let Some(expr) = condition::lower(condition, &mut table, &span, &mut diags) else {
        return false;
    };
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
        &table,
        expr,
        &ir::EvalEnv::new(&Value::Null, &run, &statics),
    )
    .unwrap_or(false)
}

#[async_trait::async_trait]
impl Step for WorkflowStep {
    const NAME: &'static str = "fabro/workflow";
    type Config = WorkflowConfig;

    async fn run(&self, config: WorkflowConfig, mut ctx: StepCtx) -> Outcome {
        let fail = |reason: String, class: &str| {
            Stage::failed(reason, class, None).into_outcome(&config.node)
        };
        let client = match ctx.require_capability::<CoordinatorInvocationClient>() {
            Ok(client) => client,
            Err(failure) => return failure.into(),
        };
        let digest = match GraphDigest::from_str(&config.child_digest) {
            Ok(digest) => digest,
            Err(error) => return fail(format!("`child_digest`: {error}"), "bad_config"),
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
                graph:   digest,
                context: context.clone(),
                secrets: SecretBindings::Inherit,
                sandbox: SandboxMode::Inherit { scope: ctx.scope },
            };
            let handle = match client.start_or_attach(request).await {
                Ok(handle) => handle,
                Err(error) => return fail(error.to_string(), "invocation"),
            };
            let result = tokio::select! {
                result = handle.result() => result,
                ctl = ctx.control.recv() => match ctl {
                    Some(Control::Deliver(_)) => {
                        // Steering has nowhere to go in a manager loop; the
                        // child is not addressable from here in phase one.
                        continue;
                    }
                    _ => {
                        return Outcome::cancelled();
                    }
                },
            };
            last_status = result.status;
            last_output = result.output.clone();
            for (key, value) in &result.context {
                context.insert(key.clone(), value.clone());
            }
            if result.status != RunStatus::Success {
                break;
            }
            if let Some(condition) = &config.stop_condition
                && stop_holds(condition, result.status, &result.context)
            {
                break;
            }
        }
        let mut stage = match last_status {
            RunStatus::Success => Stage::new("succeeded", None),
            RunStatus::Failed => Stage::failed(
                format!("the nested workflow `{}` failed", config.node),
                "child_failed",
                None,
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
