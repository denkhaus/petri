//! Sub-agents on the native backend: Pebble's supervisor, Petri's attribution.
//!
//! Pebble builds and owns the children. A native agent node hands Pebble the
//! node's [`SubagentConfig`] through [`configure`], and Pebble advertises
//! `spawn_agent`, `send_input`, `wait` and `close_agent` to the model, builds
//! each child from the parent's own client, model, environment and tool
//! middleware (so the run's tool hooks apply inside a child), never gives a
//! child the question tool, and closes every child before the parent session
//! ends. Petri starts no supervisor of its own and never reaches a child
//! except through the parent's session.
//!
//! What Petri adds is workflow attribution. Every event a child records
//! reaches the parent node's sink with the child's `session_id`, its
//! immediate `parent_session_id`, and the tree's shared `stream_id` and
//! `seq`; Pebble publishes the lifecycle facts (`SubAgentSpawned`,
//! `SubAgentTurnStarted`, `SubAgentCompleted`, `SubAgentFailed`,
//! `SubAgentClosed`) under the parent's session. The node's sink wraps each
//! event in the `pebble` envelope with the node, firing and attempt, so a
//! public event consumer reads the whole tree as `agent_activity` events of
//! the parent stage. Pebble's `PromptReport` excludes descendants, so the
//! [`Ledger`] folds the tree's events through Pebble's `SessionProjection`,
//! which keeps each descendant's account and the child lifecycle counts,
//! and the node reports them as the [`METRIC`] metric beside its own
//! `pebble.usage`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use frontend_attractor::subagents::SubagentConfig;
use pebble_coding_agent::CodingAgentBuilder;
use pebble_coding_agent::events::{CodingAgentEvent, EventSink, EventSinkError};
use pebble_coding_agent::projection::{DescendantAccount, SessionProjection};
use pebble_coding_agent::subagents::{SubagentLimits, SubagentOptions};
use serde_json::{Value, json};

/// The per-stage metric under `Metrics::custom`: `{ spawned, turns_started,
/// completed, failed, closed, usage, sessions }`, where `usage` (lithos-llm's
/// `Usage`: `tokens`, and `cost` when every message was priced) sums every
/// descendant session's committed assistant messages and `sessions` maps
/// each child session to `{ parent, provider, model, usage, messages,
/// compactions }`.
/// `provider` and `model` are the route the child runs on, as its
/// `SessionStarted` reported it (or the model of its first answer), so a
/// host prices the child's tokens at the child's own model; null when the
/// stream named neither.
pub const METRIC: &str = "pebble.subagents";

/// Give the node's agent the sub-agent tools its configuration asks for.
pub fn configure(builder: CodingAgentBuilder, config: &SubagentConfig) -> CodingAgentBuilder {
    let options = if config.enabled {
        SubagentOptions::enabled()
            .with_limits(SubagentLimits::new(config.max_open_sessions))
            .with_inherited_memory()
            .with_inherited_skills()
    } else {
        SubagentOptions::disabled()
    };
    builder.subagents(options)
}

/// One stage's account of its agent tree, folded from the events the tree
/// records by Pebble's [`SessionProjection`]. A retained thread's successor
/// node starts a new projection, so each stage reports what happened while
/// it ran the session.
#[derive(Default)]
pub struct Ledger {
    projection: Mutex<SessionProjection>,
}

impl Ledger {
    /// Record what `event` says about the tree.
    pub fn observe(&self, event: &CodingAgentEvent) {
        self.projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .apply(event);
    }

    /// How many children were spawned while this ledger watched.
    pub fn spawned(&self) -> u64 {
        self.projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .totals
            .subagent_counts
            .spawned
    }

    /// Every descendant session's account, by session id.
    pub fn sessions(&self) -> BTreeMap<String, DescendantAccount> {
        self.projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .totals
            .descendants
            .clone()
    }

    /// The [`METRIC`] value.
    pub fn metrics(&self) -> Value {
        let projection = self
            .projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let usage = projection.totals.descendant_usage();
        let sessions: serde_json::Map<String, Value> = projection
            .totals
            .descendants
            .iter()
            .map(|(session, account)| {
                (
                    session.clone(),
                    json!({
                        "parent": account.parent,
                        "provider": account.provider,
                        "model": account.model,
                        "usage": account.usage,
                        "messages": account.messages,
                        "compactions": account.compactions,
                    }),
                )
            })
            .collect();
        let counts = projection.totals.subagent_counts;
        json!({
            "spawned": counts.spawned,
            "turns_started": counts.turns_started,
            "completed": counts.completed,
            "failed": counts.failed,
            "closed": counts.closed,
            "usage": usage,
            "sessions": sessions,
        })
    }
}

/// The node's sink with the ledger in front of it: every event of the tree
/// is accounted before it is recorded, in the order Pebble delivers it.
struct Observed {
    sink:   Arc<dyn EventSink>,
    ledger: Arc<Ledger>,
}

#[async_trait::async_trait]
impl EventSink for Observed {
    async fn record(&self, event: &CodingAgentEvent) -> Result<(), EventSinkError> {
        self.ledger.observe(event);
        self.sink.record(event).await
    }
}

/// Put `ledger` in front of `sink`.
pub fn observe(sink: Arc<dyn EventSink>, ledger: Arc<Ledger>) -> Arc<dyn EventSink> {
    Arc::new(Observed { sink, ledger })
}
