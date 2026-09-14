//! Agent context compaction on the native backend.
//!
//! Fabro compacts an agent's conversation when its estimated active context
//! (the system prompt and the history) exceeds 80 percent of the model's
//! context window, keeping the six most recent turns verbatim. It has no
//! setting for this at the pinned revision; the values are hardcoded and the
//! frontend lowers them onto every agent node as `compaction`
//! (`frontend_attractor::CompactionSettings`). This module translates them into
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
//! Accounting: Pebble reports each compaction on its event stream as it
//! happens, and `CompactionCompleted` carries the summary call's usage and
//! cost. The node's sink folds the session's own `CompactionStarted` and
//! `CompactionCompleted` through [`Accounting`] as they arrive: each
//! completion is one [`EVENT`] on the spot, and the completions sum into the
//! `pebble.compaction_*` metrics. Pebble bills the summary call to the prompt
//! that compacted, so those metrics are a breakdown of `pebble.usage`. A
//! failed or cancelled compaction is neither reported nor counted, as Pebble
//! does not bill it. A child's compactions are its own: Pebble reports them
//! under the child's session and the sub-agent ledger counts them.

use std::sync::{Arc, Mutex, PoisonError};

use frontend_attractor::{DEFAULT_PRESERVE_TURNS, DEFAULT_THRESHOLD_PERCENT};
use ir::{Attempt, FiringId, StepEvent, Value};
use lithos_llm::types::Usage;
use pebble_coding_agent::events::{CodingAgentEvent, CodingEvent, CompactionReason};
use pebble_coding_agent::extensions::CompactionPolicy;
use pebble_coding_agent::{CodingAgentBuilder, CodingAgentOptions};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::ProgressSender;

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
/// compaction of the node's own session, right after Pebble's
/// `CompactionCompleted` is recorded: `{ kind, node, firing, attempt,
/// session, reason, original_turn_count, preserved_turn_count,
/// estimated_tokens_before, summary_token_estimate, tracked_file_count,
/// usage }`. `estimated_tokens_before` is the estimate the compaction's
/// `CompactionStarted` reported (null when none preceded the completion);
/// the rest is the completion's, with the summary call's `usage` (lithos-llm's
/// `Usage`: `tokens` and, when priced, `cost`). Pebble's own
/// `CompactionStarted`, `CompactionCompleted`, `CompactionFailed` and
/// `CompactionCancelled` events arrive live through the `pebble` envelope; this
/// one attributes the completion to the node and its attempt.
pub const EVENT: &str = "attractor.compaction";

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
    pub node:    SmolStr,
    pub firing:  FiringId,
    pub attempt: Attempt,
}

/// The compactions one node's own session completed, folded from Pebble's
/// stream as the events arrive. Shared between the node's sink, which folds,
/// and the session, which reports the metrics.
#[derive(Debug, Default)]
pub struct Accounting {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The estimate the session's compaction in progress reported when it
    /// started, until it ends.
    started: Option<usize>,
    count:   u64,
    /// The completions' summary calls, summed; priced only when each was.
    usage:   Usage,
}

/// One completed compaction of the session, as [`EVENT`] reports it.
#[derive(Debug, PartialEq, Eq)]
struct Completed {
    session:                 String,
    reason:                  CompactionReason,
    original_turn_count:     usize,
    preserved_turn_count:    usize,
    estimated_tokens_before: Option<usize>,
    summary_token_estimate:  usize,
    tracked_file_count:      usize,
    usage:                   Usage,
}

impl Accounting {
    /// Fold `event`; when it completes one of the session's own compactions,
    /// report that as [`EVENT`] through `sender`. A send that fails means
    /// the driver stopped taking this attempt's progress, which the
    /// attempt's own outcome reports.
    pub async fn observe(
        &self,
        event: &CodingAgentEvent,
        sender: &ProgressSender,
        at: &Attribution,
    ) {
        let Some(completed) = self.fold(event) else {
            return;
        };
        let _ = sender
            .send(StepEvent::Custom(json!({
                "kind": EVENT,
                "node": at.node,
                "firing": at.firing,
                "attempt": at.attempt,
                "session": completed.session,
                "reason": completed.reason,
                "original_turn_count": completed.original_turn_count,
                "preserved_turn_count": completed.preserved_turn_count,
                "estimated_tokens_before": completed.estimated_tokens_before,
                "summary_token_estimate": completed.summary_token_estimate,
                "tracked_file_count": completed.tracked_file_count,
                "usage": completed.usage,
            })))
            .await;
    }

    /// What `event` says about the session's own compactions: the estimate
    /// a start reported is kept for the completion that follows it; a
    /// completion is counted, its usage added, and returned; a failure or a
    /// cancellation only ends the compaction in progress.
    fn fold(&self, event: &CodingAgentEvent) -> Option<Completed> {
        if event.parent_session_id.is_some() {
            return None;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &event.event {
            CodingEvent::CompactionStarted {
                estimated_tokens, ..
            } => {
                state.started = Some(*estimated_tokens);
                None
            }
            CodingEvent::CompactionCompleted {
                original_turn_count,
                preserved_turn_count,
                summary_token_estimate,
                tracked_file_count,
                reason,
                usage,
            } => {
                state.count += 1;
                state.usage = state.usage.saturating_add(*usage);
                Some(Completed {
                    session:                 event.session_id.clone(),
                    reason:                  *reason,
                    original_turn_count:     *original_turn_count,
                    preserved_turn_count:    *preserved_turn_count,
                    estimated_tokens_before: state.started.take(),
                    summary_token_estimate:  *summary_token_estimate,
                    tracked_file_count:      *tracked_file_count,
                    usage:                   *usage,
                })
            }
            CodingEvent::CompactionFailed { .. } | CodingEvent::CompactionCancelled { .. } => {
                state.started = None;
                None
            }
            _ => None,
        }
    }

    /// `pebble.compactions` and `pebble.compaction_usage`.
    #[must_use]
    pub fn metrics(&self) -> [(SmolStr, Value); 2] {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        [
            ("pebble.compactions".into(), json!(state.count)),
            ("pebble.compaction_usage".into(), json!(state.usage)),
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use lithos_llm::types::{Cost, CostSource, TokenCounts};
    use pebble_coding_agent::events::{ErrorData, ErrorKind};
    use steps::Progress;

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

    fn usage(input: u64, output: u64, usd_micros: Option<u64>) -> Usage {
        Usage {
            tokens: TokenCounts {
                input,
                output,
                ..TokenCounts::default()
            },
            cost:   usd_micros.map(|usd_micros| Cost {
                usd_micros,
                source: CostSource::Catalog,
            }),
        }
    }

    fn started(estimated_tokens: usize) -> CodingEvent {
        CodingEvent::CompactionStarted {
            estimated_tokens,
            context_window_size: 200_000,
            reason: CompactionReason::Threshold,
        }
    }

    fn completed(usage: Usage) -> CodingEvent {
        CodingEvent::CompactionCompleted {
            original_turn_count: 10,
            preserved_turn_count: 7,
            summary_token_estimate: 12,
            tracked_file_count: 1,
            reason: CompactionReason::Threshold,
            usage,
        }
    }

    fn root(event: CodingEvent) -> CodingAgentEvent {
        CodingAgentEvent::new("root", event, SystemTime::UNIX_EPOCH)
    }

    fn child(event: CodingEvent) -> CodingAgentEvent {
        let mut envelope = CodingAgentEvent::new("child", event, SystemTime::UNIX_EPOCH);
        envelope.parent_session_id = Some("root".into());
        envelope
    }

    #[test]
    fn a_completion_is_reported_with_the_estimate_its_start_carried() {
        let accounting = Accounting::default();
        assert_eq!(accounting.fold(&root(started(160_001))), None);
        let reported = accounting
            .fold(&root(completed(usage(70, 7, Some(21)))))
            .expect("the completion");
        assert_eq!(reported, Completed {
            session:                 "root".into(),
            reason:                  CompactionReason::Threshold,
            original_turn_count:     10,
            preserved_turn_count:    7,
            estimated_tokens_before: Some(160_001),
            summary_token_estimate:  12,
            tracked_file_count:      1,
            usage:                   usage(70, 7, Some(21)),
        });
        // A second compaction sums, and reads its own start.
        assert_eq!(accounting.fold(&root(started(170_000))), None);
        let second = accounting
            .fold(&root(completed(usage(30, 3, Some(4)))))
            .expect("the second completion");
        assert_eq!(second.estimated_tokens_before, Some(170_000));
        let metrics = accounting.metrics();
        assert_eq!(metrics[0], ("pebble.compactions".into(), json!(2)));
        assert_eq!(metrics[1].0, "pebble.compaction_usage");
        assert_eq!(metrics[1].1, json!(usage(100, 10, Some(25))));
        // A third, unpriced, leaves the tokens summed and the cost unknown.
        let third = accounting
            .fold(&root(completed(usage(5, 1, None))))
            .expect("the third completion");
        assert_eq!(third.usage, usage(5, 1, None));
        assert_eq!(accounting.metrics()[1].1, json!(usage(105, 11, None)));
    }

    #[test]
    fn a_failure_or_a_cancellation_ends_the_compaction_without_counting_it() {
        let accounting = Accounting::default();
        assert_eq!(accounting.fold(&root(started(160_001))), None);
        assert_eq!(
            accounting.fold(&root(CodingEvent::CompactionFailed {
                reason: CompactionReason::Threshold,
                error:  ErrorData::new(ErrorKind::Compaction, "summary refused"),
                usage:  Some(usage(70, 0, Some(5))),
            })),
            None
        );
        assert_eq!(accounting.fold(&root(started(160_002))), None);
        assert_eq!(
            accounting.fold(&root(CodingEvent::CompactionCancelled {
                reason: CompactionReason::Threshold,
            })),
            None
        );
        // A completion that no start preceded reports no estimate.
        let reported = accounting
            .fold(&root(completed(usage(70, 7, None))))
            .expect("the completion");
        assert_eq!(reported.estimated_tokens_before, None);
        let metrics = accounting.metrics();
        assert_eq!(metrics[0].1, json!(1));
        assert_eq!(
            metrics[1].1["tokens"]["input"], 70,
            "the failed call is not billed"
        );
        assert!(metrics[1].1.get("cost").is_none(), "{:?}", metrics[1].1);
    }

    #[test]
    fn a_childs_compaction_is_the_childs_own() {
        let accounting = Accounting::default();
        assert_eq!(accounting.fold(&child(started(160_001))), None);
        assert_eq!(
            accounting.fold(&child(completed(usage(70, 7, Some(21))))),
            None
        );
        assert_eq!(accounting.metrics()[0].1, json!(0));
    }

    #[tokio::test]
    async fn the_event_is_sent_as_the_completion_arrives() {
        let (sender, mut progress) = ProgressSender::channel(4);
        let at = Attribution {
            node:    "a".into(),
            firing:  FiringId::new(4),
            attempt: Attempt::FIRST,
        };
        let accounting = Accounting::default();
        accounting
            .observe(&root(started(160_001)), &sender, &at)
            .await;
        assert!(
            progress.try_recv().is_err(),
            "a start alone reports nothing"
        );
        accounting
            .observe(&root(completed(usage(70, 7, Some(21)))), &sender, &at)
            .await;
        let Progress {
            event: StepEvent::Custom(payload),
            ..
        } = progress.try_recv().expect("the completion is reported")
        else {
            panic!("a custom event");
        };
        assert_eq!(payload["kind"], EVENT);
        assert_eq!(payload["node"], "a");
        assert_eq!(payload["session"], "root");
        assert_eq!(payload["reason"], "threshold");
        assert_eq!(payload["estimated_tokens_before"], 160_001);
        assert_eq!(payload["original_turn_count"], 10);
        assert_eq!(payload["usage"]["tokens"]["input"], 70);
        assert_eq!(payload["usage"]["cost"]["usd_micros"], 21);
        assert_eq!(payload["usage"]["cost"]["source"], "catalog");
        assert!(payload.get("cost_usd_micros").is_none());
        assert!(payload.get("summary_truncated").is_none());
    }
}
