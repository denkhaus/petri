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
//! [`Ledger`] sums what the children spent from those events and the node
//! reports it as the [`METRIC`] metric beside its own `pebble.usage`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use frontend_fabro::subagents::SubagentConfig;
use pebble_coding_agent::CodingAgentBuilder;
use pebble_coding_agent::events::{
    CodingAgentEvent, CodingEvent, EventSink, EventSinkError, TokenUsage,
};
use pebble_coding_agent::subagents::{SubagentLimits, SubagentOptions};
use serde_json::{Value, json};

/// The per-stage metric under `Metrics::custom`: `{ spawned, turns_started,
/// completed, failed, closed, usage, cost_usd_micros, sessions }`, where
/// `usage` and `cost_usd_micros` sum every descendant session's committed
/// assistant messages and `sessions` maps each child session to `{ parent,
/// usage, cost_usd_micros, messages }`.
pub const METRIC: &str = "pebble.subagents";

/// Give the node's agent the sub-agent tools its configuration asks for.
pub fn configure(builder: CodingAgentBuilder, config: &SubagentConfig) -> CodingAgentBuilder {
    let options = if config.enabled {
        SubagentOptions::enabled().with_limits(SubagentLimits::new(config.max_open_sessions))
    } else {
        SubagentOptions::disabled()
    };
    builder.subagents(options)
}

/// What one child session spent, as its own events reported it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChildAccount {
    /// The session that spawned it.
    pub parent:          String,
    pub usage:           TokenUsage,
    pub cost_usd_micros: Option<u64>,
    /// Committed assistant messages.
    pub messages:        u64,
}

#[derive(Default)]
struct State {
    spawned:       u64,
    turns_started: u64,
    completed:     u64,
    failed:        u64,
    closed:        u64,
    sessions:      BTreeMap<String, ChildAccount>,
}

/// One stage's account of its agent tree, kept from the events the tree
/// records. A retained thread's successor node starts a new ledger, so each
/// stage reports what happened while it ran the session.
#[derive(Default)]
pub struct Ledger {
    state: Mutex<State>,
}

impl Ledger {
    /// Record what `event` says about the tree.
    pub fn observe(&self, event: &CodingAgentEvent) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &event.event {
            CodingEvent::SubAgentSpawned { .. } => state.spawned += 1,
            CodingEvent::SubAgentTurnStarted { .. } => state.turns_started += 1,
            CodingEvent::SubAgentCompleted { .. } => state.completed += 1,
            CodingEvent::SubAgentFailed { .. } => state.failed += 1,
            CodingEvent::SubAgentClosed { .. } => state.closed += 1,
            CodingEvent::AssistantMessage {
                usage,
                cost_usd_micros,
                ..
            } => {
                // A message with a parent is a descendant's; the root's own
                // usage is the prompt report's.
                if let Some(parent) = &event.parent_session_id {
                    let account = state
                        .sessions
                        .entry(event.session_id.clone())
                        .or_insert_with(|| ChildAccount {
                            parent: parent.clone(),
                            ..ChildAccount::default()
                        });
                    account.usage = account.usage.saturating_add(*usage);
                    if let Some(cost) = cost_usd_micros {
                        account.cost_usd_micros =
                            Some(account.cost_usd_micros.unwrap_or(0).saturating_add(*cost));
                    }
                    account.messages += 1;
                }
            }
            _ => {}
        }
    }

    /// How many children were spawned while this ledger watched.
    pub fn spawned(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spawned
    }

    /// Every descendant session's account, by session id.
    pub fn sessions(&self) -> BTreeMap<String, ChildAccount> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sessions
            .clone()
    }

    /// The [`METRIC`] value.
    pub fn metrics(&self) -> Value {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut usage = TokenUsage::default();
        let mut cost: Option<u64> = None;
        let mut sessions = serde_json::Map::new();
        for (session, account) in &state.sessions {
            usage = usage.saturating_add(account.usage);
            if let Some(child_cost) = account.cost_usd_micros {
                cost = Some(cost.unwrap_or(0).saturating_add(child_cost));
            }
            sessions.insert(
                session.clone(),
                json!({
                    "parent": account.parent,
                    "usage": account.usage,
                    "cost_usd_micros": account.cost_usd_micros,
                    "messages": account.messages,
                }),
            );
        }
        json!({
            "spawned": state.spawned,
            "turns_started": state.turns_started,
            "completed": state.completed,
            "failed": state.failed,
            "closed": state.closed,
            "usage": usage,
            "cost_usd_micros": cost,
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
