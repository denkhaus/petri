//! The run's tool hooks at the two boundaries ACP offers: a permission
//! request (`pre_tool_use`) and a tool call the agent reports finished
//! (`post_tool_use`, `post_tool_use_failure`). See the module doc of
//! [`super`] for what each boundary can and cannot see.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use execution::hooks::{HookDecision, HookPoint};
use frontend_attractor::hooks::HookEvent;
use ir::{StepEvent, Value};
use steps::ProgressSender;

use super::BACKEND;
use crate::hooks::{ToolHookBinding, ToolPayload};

/// The hook service bound to one ACP node, and what it has already warned
/// about.
pub struct AcpHooks {
    binding: ToolHookBinding,
    /// Configured tool hooks by event, so a boundary gap is reported once per
    /// hook.
    pre:     Vec<String>,
    post:    Vec<String>,
    warned:  Mutex<BTreeSet<String>>,
}

impl AcpHooks {
    pub(crate) fn new(binding: ToolHookBinding, pre: Vec<String>, post: Vec<String>) -> Self {
        Self {
            binding,
            pre,
            post,
            warned: Mutex::new(BTreeSet::new()),
        }
    }

    /// Whether a `pre_tool_use` hook is configured: the permission policy
    /// then allows once, never always, so every later call still asks.
    pub fn has_pre(&self) -> bool {
        !self.pre.is_empty()
    }

    /// The warnings to emit before the agent starts: what this backend can
    /// and cannot see for each configured tool hook.
    pub fn known_gaps(&self) -> Vec<StepEvent> {
        let mut out = Vec::new();
        for hook in &self.pre {
            out.push(self.binding.warning(
                BACKEND,
                hook,
                HookEvent::PreToolUse,
                "session/request_permission",
                "the ACP agent decides which tool calls ask for permission; this hook runs only \
                 for those, and a tool call the agent does not ask about is not intercepted",
            ));
        }
        for hook in &self.post {
            out.push(self.binding.warning(
                BACKEND,
                hook,
                HookEvent::PostToolUse,
                "session/update",
                "the ACP agent decides which tool calls it reports; this hook runs for the calls \
                 it reports finished (`tool_call_update` with status `completed` or `failed`), \
                 and a call the agent never reports is not seen",
            ));
        }
        out
    }

    /// Ask the `pre_tool_use` hooks about a permission request.
    /// `Some(reason)` blocks.
    pub(super) async fn pre_tool(&self, call: &ToolCall, logs: &ProgressSender) -> Option<String> {
        let payload = ToolPayload {
            tool_name: call.name.clone(),
            tool_call_id: call.id.clone(),
            tool_input: call.input.clone(),
            ..ToolPayload::default()
        };
        match self
            .binding
            .ask(HookPoint::BeforeToolUse, payload, logs)
            .await
        {
            HookDecision::Block { reason } => Some(reason),
            _ => None,
        }
    }

    /// A tool call the agent reported finished: `post_tool_use` with its
    /// output, or `post_tool_use_failure` with its error. The decision is
    /// ignored, as Fabro ignores it.
    pub(super) async fn post_tool(
        &self,
        call: &ToolCall,
        finished: &Finished,
        logs: &ProgressSender,
    ) {
        if self.post.is_empty() {
            return;
        }
        let (point, payload) = match finished {
            Finished::Completed { output } => (HookPoint::AfterToolUse, ToolPayload {
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
                tool_output: output.clone(),
                ..ToolPayload::default()
            }),
            Finished::Failed { error } => (HookPoint::AfterToolFailure, ToolPayload {
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
                error_message: error.clone(),
                ..ToolPayload::default()
            }),
        };
        let _ = self.binding.ask(point, payload, logs).await;
    }

    /// A tool call the agent ran without asking permission: the configured
    /// `pre_tool_use` hooks could not run for it. Warn once per hook and
    /// tool.
    pub(super) async fn unintercepted(&self, tool: &str, logs: &ProgressSender) {
        for hook in &self.pre {
            let key = format!("{hook}:{tool}");
            let first = self
                .warned
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key);
            if !first {
                continue;
            }
            let _ = logs
                .send(self.binding.warning(
                    BACKEND,
                    hook,
                    HookEvent::PreToolUse,
                    "session/update",
                    &format!(
                        "the agent ran `{tool}` without a permission request; the hook did not \
                         run for it"
                    ),
                ))
                .await;
        }
    }
}

/// A tool call the agent named, by its id: what the hooks call it, and
/// whether the agent asked permission for it.
#[derive(Clone, Debug)]
pub(super) struct ToolCall {
    pub(super) id:    Option<String>,
    pub(super) name:  String,
    pub(super) input: Option<Value>,
    pub(super) asked: bool,
}

impl Default for ToolCall {
    /// A call nothing has named yet: `tool` to the hooks, never asked.
    fn default() -> Self {
        Self {
            id:    None,
            name:  "tool".to_owned(),
            input: None,
            asked: false,
        }
    }
}

impl ToolCall {
    /// Take what a `tool_call`, `tool_call_update` or permission request's
    /// `toolCall` says about the call over what was remembered: its id, its
    /// `title` (else its `kind`) as the name, and its `rawInput`. A field
    /// the value leaves out keeps its remembered value; `asked` is never
    /// unset.
    pub(super) fn merge(&mut self, value: &Value) {
        if let Some(id) = value.get("toolCallId").and_then(Value::as_str) {
            self.id = Some(id.to_owned());
        }
        if let Some(name) = value
            .get("title")
            .or_else(|| value.get("kind"))
            .and_then(Value::as_str)
        {
            name.clone_into(&mut self.name);
        }
        if let Some(input) = value.get("rawInput") {
            self.input = Some(input.clone());
        }
    }
}

/// How a reported tool call ended.
pub(super) enum Finished {
    Completed { output: Option<String> },
    Failed { error: Option<String> },
}

/// What a finished tool call carries for the post-tool hooks, as Fabro's
/// `tool_output`: the text of its content blocks, else `rawOutput` as JSON,
/// else nothing.
pub(super) fn tool_output(update: &Value) -> Option<String> {
    let text: Vec<&str> = update
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| {
            let content = block.get("content")?;
            (content.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| content.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect();
    if !text.is_empty() {
        return Some(text.join("\n"));
    }
    match update.get("rawOutput") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Null) | None => None,
        Some(other) => serde_json::to_string(other).ok(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_merge_takes_the_title_over_the_kind_and_keeps_what_the_update_leaves_out() {
        let mut call = ToolCall::default();
        assert_eq!(call.name, "tool", "an unnamed call is `tool` to the hooks");
        call.merge(&json!({
            "toolCallId": "c1",
            "kind": "edit",
            "title": "Edit main.rs",
            "rawInput": { "path": "main.rs" },
        }));
        assert_eq!(call.id.as_deref(), Some("c1"));
        assert_eq!(call.name, "Edit main.rs", "the title wins over the kind");
        assert_eq!(call.input, Some(json!({ "path": "main.rs" })));

        call.merge(&json!({ "toolCallId": "c1", "status": "completed" }));
        assert_eq!(
            call.name, "Edit main.rs",
            "a known name survives an update without one"
        );
        assert_eq!(call.input, Some(json!({ "path": "main.rs" })));

        call.merge(&json!({ "kind": "read", "rawInput": null }));
        assert_eq!(
            call.name, "read",
            "a kind names the call when there is no title"
        );
        assert_eq!(
            call.input,
            Some(Value::Null),
            "a present `rawInput` replaces the input"
        );
    }

    #[test]
    fn asked_survives_a_merge() {
        let mut call = ToolCall {
            asked: true,
            ..ToolCall::default()
        };
        call.merge(&json!({ "toolCallId": "c1", "title": "read", "status": "in_progress" }));
        assert!(call.asked);
    }

    #[test]
    fn tool_output_prefers_text_content_over_raw_output() {
        let update = json!({
            "content": [
                { "type": "content", "content": { "type": "text", "text": "one" } },
                { "type": "diff", "path": "a", "newText": "b" },
                { "type": "content", "content": { "type": "text", "text": "two" } },
            ],
            "rawOutput": { "ignored": true },
        });
        assert_eq!(tool_output(&update).as_deref(), Some("one\ntwo"));
        assert_eq!(
            tool_output(&json!({ "rawOutput": "plain" })).as_deref(),
            Some("plain")
        );
        assert_eq!(
            tool_output(&json!({ "rawOutput": { "a": 1 } })).as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(tool_output(&json!({ "status": "completed" })), None);
    }
}
