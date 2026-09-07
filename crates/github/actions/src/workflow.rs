//! A reusable workflow runs through the ordinary invocation coordinator.

use std::collections::BTreeMap;

use execution::{
    CallSite, CoordinatorInvocationClient, GraphDigest, InvocationClient, InvocationRequest,
    SandboxMode, SecretBinding, SecretBindings,
};
use ir::{FailureClass, FailureInfo, Outcome, RunStatus, Status, Value};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Step, StepCtx};

const CALL_CLASS: FailureClass = FailureClass::new_static("workflow_call");

pub struct WorkflowCallStep;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCallConfig {
    graph:   GraphDigest,
    context: BTreeMap<SmolStr, Value>,
    /// `None` inherits the parent's view. An explicit map contains parent
    /// names or `None` for declared optional secrets that were not supplied.
    secrets: Option<BTreeMap<SmolStr, Option<SmolStr>>>,
}

#[async_trait::async_trait]
impl Step for WorkflowCallStep {
    const NAME: &'static str = frontend_gha::WORKFLOW_CALL_KIND;
    type Config = WorkflowCallConfig;

    async fn run(&self, config: Self::Config, mut ctx: StepCtx) -> Outcome {
        let client = match ctx.require_capability::<CoordinatorInvocationClient>() {
            Ok(client) => client,
            Err(failure) => return failure.into(),
        };
        let secrets = config.secrets.map_or(SecretBindings::Inherit, |bindings| {
            SecretBindings::Explicit(
                bindings
                    .into_iter()
                    .map(|(name, parent)| {
                        (
                            name,
                            parent.map_or(SecretBinding::Empty, SecretBinding::Parent),
                        )
                    })
                    .collect(),
            )
        });
        let request = InvocationRequest {
            site: CallSite {
                firing:  ctx.firing,
                attempt: ctx.attempt,
                slot:    SmolStr::new_static("workflow"),
            },
            graph: config.graph,
            context: config.context,
            secrets,
            sandbox: SandboxMode::Isolated,
            admission: None,
        };
        let mut child = match client.start_or_attach(request).await {
            Ok(child) => child,
            Err(error) => return failure(error.to_string(), Value::Null),
        };
        let Some(result) = child.result_with_control(&mut ctx.control).await else {
            return Outcome::cancelled();
        };
        // A valid child projection owns GitHub's result, including `skipped`.
        // Coordinator status is the fallback when no summary could run.
        if result.output.get("outputs").is_some_and(Value::is_object) {
            match result.output.get("result").and_then(Value::as_str) {
                Some("success" | "skipped") => return Outcome::success(result.output),
                Some("failure") => {
                    return failure("the called workflow failed".into(), result.output);
                }
                Some("cancelled") => return Outcome::new(Status::Cancelled, result.output),
                _ => {}
            }
        }
        match result.status {
            RunStatus::Cancelled => Outcome::new(
                Status::Cancelled,
                json!({"result":"cancelled", "outputs":{}}),
            ),
            RunStatus::Failed => failure(
                result.failure.map_or_else(
                    || "the called workflow failed before producing a summary".into(),
                    |failure| failure.message,
                ),
                json!({"result":"failure", "outputs":{}}),
            ),
            RunStatus::Success => failure(
                "the called workflow returned an invalid summary".into(),
                Value::Null,
            ),
        }
    }
}

fn failure(message: String, output: Value) -> Outcome {
    Outcome::new(
        Status::Failure(FailureInfo::new(message).with_class(CALL_CLASS)),
        output,
    )
}
