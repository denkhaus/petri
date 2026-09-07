//! Fabro's tool hooks at Pebble's tool boundary.
//!
//! [`ToolHooks`] is a Pebble `ToolMiddleware`. Before a tool runs it asks
//! the hook service at `BeforeToolUse`; a block returns a denied outcome
//! without calling the next layer, so the tool never runs and the model sees
//! the reason. After the call it asks `AfterToolUse` or `AfterToolFailure`
//! with the outcome and discards the decision, as Fabro does. Every report
//! that says something is recorded on the step's progress channel as a
//! [`super::REPORT_EVENT`], attributed to the node, firing and attempt the
//! middleware was bound with, so a retained session rebinds a new instance
//! for each node that continues it.

use std::sync::Arc;

use execution::hooks::{HookDecision, HookPoint, HookRequest, HookService};
use frontend_fabro::hooks::HookEvent;
use ir::{Attempt, FiringId, StepEvent, Value};
use pebble_agent::{
    ToolCallNext, ToolCallRequest, ToolErrorKind, ToolMiddleware, ToolOutcome, ToolSystemError,
};
use runtime::driver::FiringView;
use serde_json::json;
use smol_str::SmolStr;
use tokio::sync::mpsc;

use super::{ToolPayload, report_event};

/// The middleware, bound to one node's firing.
pub struct ToolHooks {
    service: Arc<dyn HookService>,
    view:    Arc<FiringView>,
    logs:    mpsc::Sender<StepEvent>,
    node:    SmolStr,
    firing:  FiringId,
    attempt: Attempt,
}

impl ToolHooks {
    pub fn new(
        service: Arc<dyn HookService>,
        view: Arc<FiringView>,
        logs: mpsc::Sender<StepEvent>,
        node: SmolStr,
        firing: FiringId,
        attempt: Attempt,
    ) -> Self {
        Self {
            service,
            view,
            logs,
            node,
            firing,
            attempt,
        }
    }

    async fn ask(&self, point: HookPoint, payload: ToolPayload) -> HookDecision {
        let report = self
            .service
            .run(HookRequest {
                point,
                view: self.view.clone(),
                outcome: None,
                routes: Vec::new(),
                payload: serde_json::to_value(&payload).unwrap_or(Value::Null),
            })
            .await;
        if !report.is_silent() {
            let event = match point {
                HookPoint::BeforeToolUse => HookEvent::PreToolUse,
                HookPoint::AfterToolUse => HookEvent::PostToolUse,
                _ => HookEvent::PostToolUseFailure,
            };
            let _ = self
                .logs
                .send(report_event(
                    &self.node,
                    self.firing,
                    self.attempt,
                    event,
                    &report,
                ))
                .await;
        }
        report.decision
    }
}

/// The text a tool output carries, as Fabro's `tool_output`: the text parts,
/// or the content as JSON when there is none.
fn output_text(outcome: &ToolOutcome) -> Option<String> {
    match outcome {
        ToolOutcome::Success { output, .. } => {
            let text = output.text();
            if text.is_empty() {
                serde_json::to_string(output.content()).ok()
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

#[async_trait::async_trait]
impl ToolMiddleware for ToolHooks {
    async fn call(
        &self,
        request: ToolCallRequest,
        next: ToolCallNext<'_>,
    ) -> Result<ToolOutcome, ToolSystemError> {
        let tool_name = request.descriptor().id().to_string();
        let tool_call_id = request.call().id.clone();
        let tool_input = request.call().input.to_value().ok();
        let decision = self
            .ask(HookPoint::BeforeToolUse, ToolPayload {
                tool_name: tool_name.clone(),
                tool_call_id: Some(tool_call_id.clone()),
                tool_input,
                ..ToolPayload::default()
            })
            .await;
        if let HookDecision::Block { reason } = decision {
            // The next layer is never called: the effect cannot happen.
            return Ok(ToolOutcome::failure(ToolErrorKind::Denied, reason));
        }
        let outcome = next.run(request).await?;
        let (point, payload) = match &outcome {
            ToolOutcome::Success { .. } => (HookPoint::AfterToolUse, ToolPayload {
                tool_name,
                tool_call_id: Some(tool_call_id),
                tool_output: output_text(&outcome),
                ..ToolPayload::default()
            }),
            ToolOutcome::Failure { message, .. } => (HookPoint::AfterToolFailure, ToolPayload {
                tool_name,
                tool_call_id: Some(tool_call_id),
                error_message: Some(message.clone()),
                ..ToolPayload::default()
            }),
            _ => (HookPoint::AfterToolFailure, ToolPayload {
                tool_name,
                tool_call_id: Some(tool_call_id),
                ..ToolPayload::default()
            }),
        };
        // Post-tool decisions are ignored, as Fabro ignores them.
        let _ = self.ask(point, payload).await;
        Ok(outcome)
    }
}

/// The `StepEvent::Custom` that says a backend could not enforce a hook.
pub fn unenforceable(
    node: &str,
    firing: FiringId,
    attempt: Attempt,
    backend: &str,
    hook: &str,
    event: HookEvent,
    boundary: &str,
    message: &str,
) -> StepEvent {
    super::warning_event(
        node, firing, attempt, backend, hook, event, boundary, message,
    )
}

/// The JSON a warning carries, for tests.
pub fn warning_json(backend: &str, hook: &str, event: HookEvent, boundary: &str) -> Value {
    json!({ "backend": backend, "hook": hook, "event": event.as_str(), "boundary": boundary })
}
