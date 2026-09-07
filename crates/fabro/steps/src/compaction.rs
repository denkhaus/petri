//! Agent context compaction on the native backend.
//!
//! Fabro compacts an agent's conversation when its estimated active context
//! (the system prompt and the history) exceeds 80 percent of the model's
//! context window, keeping the six most recent turns verbatim. It has no
//! setting for this at the pinned revision; the values are hardcoded and the
//! frontend lowers them onto every agent node as `compaction`
//! (`frontend_fabro::CompactionSettings`). This module translates them into
//! Pebble's options. Pebble owns the estimate (the last reported usage plus a
//! local delta, or a local estimate when no usage was reported), the trigger
//! (strictly above `window * percent / 100`, checked before and after each
//! model turn), the safe cut that never separates a tool call from its
//! result, the summary call, the history replacement and the terminal
//! compaction events. Petri never rewrites the committed history.
//!
//! An embedding host may supply the summary through Pebble's
//! `CompactionPolicy`: register a [`CompactionPolicyHandle`] capability and
//! every native session, resumed ones included, installs it. Pebble still
//! chooses the cut, validates the summary and records the outcome.
//!
//! Accounting: at the pinned Pebble a prompt's usage excludes the summary
//! call, but the call's usage and cost are recorded on the `Compaction` turn
//! Pebble puts in the history. After each prompt settles, [`Accounting`]
//! reads the turns that appeared, emits one [`EVENT`] per compaction with
//! that usage, and sums them into the `pebble.compaction_*` metrics beside
//! `pebble.usage`.

use std::sync::Arc;

use frontend_fabro::{DEFAULT_PRESERVE_TURNS, DEFAULT_THRESHOLD_PERCENT};
use ir::{Attempt, FiringId, StepEvent, Value};
use pebble_coding_agent::events::TokenUsage;
use pebble_coding_agent::extensions::CompactionPolicy;
use pebble_coding_agent::state::Message;
use pebble_coding_agent::{CodingAgent, CodingAgentBuilder, CodingAgentOptions};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use tokio::sync::mpsc;

/// The `compaction` object of an agent node's config, as the frontend lowers
/// it. Absent values are Fabro's.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionSettings {
    pub enabled:           bool,
    pub threshold_percent: u8,
    pub preserve_turns:    u32,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled:           true,
            threshold_percent: DEFAULT_THRESHOLD_PERCENT,
            preserve_turns:    DEFAULT_PRESERVE_TURNS,
        }
    }
}

/// Host capability: an embedding application's summary generator, installed
/// on every native session through Pebble's `CompactionPolicy` interface.
#[derive(Clone)]
pub struct CompactionPolicyHandle(pub Arc<dyn CompactionPolicy>);

/// The `kind` of the `StepEvent::Custom` payload emitted once per completed
/// compaction, after the prompt it happened in settles: `{ kind, node,
/// firing, attempt, session, reason, original_turn_count,
/// preserved_turn_count, estimated_tokens_before, summary_token_estimate,
/// tracked_file_count, summary_truncated, usage, cost_usd_micros }`.
/// Pebble's own `CompactionStarted`, `CompactionCompleted`,
/// `CompactionFailed` and `CompactionCancelled` events arrive live through
/// the `pebble` envelope; this one adds the summary call's usage, which those
/// do not carry.
pub const EVENT: &str = "fabro.compaction";

/// Fabro's compaction settings as Pebble options.
#[must_use]
pub fn options(options: CodingAgentOptions, settings: &CompactionSettings) -> CodingAgentOptions {
    options
        .with_context_compaction(settings.enabled)
        .with_compaction_threshold_percent(usize::from(settings.threshold_percent))
        .with_compaction_preserve_turns(settings.preserve_turns as usize)
}

/// Install the host's summary policy, when the run has one.
pub fn install(
    builder: CodingAgentBuilder,
    policy: Option<Arc<CompactionPolicyHandle>>,
) -> CodingAgentBuilder {
    match policy {
        Some(handle) => builder.compaction_policy(handle.0.clone()),
        None => builder,
    }
}

/// Where a session's compaction events are attributed.
pub struct Attribution {
    pub sender:  mpsc::Sender<StepEvent>,
    pub node:    SmolStr,
    pub firing:  FiringId,
    pub attempt: Attempt,
}

/// The compactions one node's session performed, read from the history
/// after each prompt.
pub struct Accounting {
    /// Compaction turns already reported (or inherited from a resumed
    /// export, which a predecessor reported).
    seen:  usize,
    count: u64,
    usage: TokenUsage,
    cost:  Option<u64>,
}

impl Accounting {
    /// Start counting after the turns `agent` already holds.
    #[must_use]
    pub fn new(agent: &CodingAgent) -> Self {
        Self {
            seen:  compaction_turns(agent).len(),
            count: 0,
            usage: TokenUsage::default(),
            cost:  None,
        }
    }

    /// Report every compaction since the last call and add its usage.
    pub async fn settle(&mut self, agent: &CodingAgent, at: &Attribution) {
        let session = agent.snapshot().session_id().to_string();
        let turns = compaction_turns(agent);
        for turn in turns.iter().skip(self.seen) {
            let Message::Compaction {
                reason,
                original_turn_count,
                preserved_turn_count,
                estimated_tokens_before,
                summary_token_estimate,
                tracked_file_count,
                summary_truncated,
                usage,
                cost_usd_micros,
                ..
            } = turn
            else {
                continue;
            };
            self.count += 1;
            self.usage = self.usage.saturating_add(*usage);
            if let Some(cost) = cost_usd_micros {
                self.cost = Some(self.cost.unwrap_or(0).saturating_add(*cost));
            }
            let _ = at
                .sender
                .send(StepEvent::Custom(json!({
                    "kind": EVENT,
                    "node": at.node,
                    "firing": at.firing,
                    "attempt": at.attempt,
                    "session": session,
                    "reason": reason,
                    "original_turn_count": original_turn_count,
                    "preserved_turn_count": preserved_turn_count,
                    "estimated_tokens_before": estimated_tokens_before,
                    "summary_token_estimate": summary_token_estimate,
                    "tracked_file_count": tracked_file_count,
                    "summary_truncated": summary_truncated,
                    "usage": usage,
                    "cost_usd_micros": cost_usd_micros,
                })))
                .await;
        }
        self.seen = turns.len();
    }

    /// `pebble.compactions`, `pebble.compaction_usage` and
    /// `pebble.compaction_cost_usd_micros`.
    #[must_use]
    pub fn metrics(&self) -> [(SmolStr, Value); 3] {
        [
            ("pebble.compactions".into(), json!(self.count)),
            ("pebble.compaction_usage".into(), json!(self.usage)),
            ("pebble.compaction_cost_usd_micros".into(), json!(self.cost)),
        ]
    }
}

fn compaction_turns(agent: &CodingAgent) -> Vec<Message> {
    agent
        .history()
        .turns()
        .iter()
        .filter(|turn| matches!(turn, Message::Compaction { .. }))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_to_fabros_values_and_refuse_unknown_keys() {
        let settings: CompactionSettings = serde_json::from_value(json!({})).expect("defaults");
        assert_eq!(settings, CompactionSettings::default());
        assert!(settings.enabled);
        assert_eq!(settings.threshold_percent, 80);
        assert_eq!(settings.preserve_turns, 6);
        let explicit: CompactionSettings =
            serde_json::from_value(json!({"enabled": false, "threshold_percent": 50}))
                .expect("partial");
        assert!(!explicit.enabled);
        assert_eq!(explicit.threshold_percent, 50);
        assert_eq!(explicit.preserve_turns, 6);
        assert!(serde_json::from_value::<CompactionSettings>(json!({"window": 1})).is_err());
    }
}
