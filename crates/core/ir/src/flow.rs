//! Values that flow while a graph runs: tokens, outcomes, run context, control
//! signals.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use crate::ids::{EdgeId, FiringId, Generation};
use crate::splice::SpliceRequest;

/// One unit of flow, sitting on an edge and waiting for a join.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub edge:       EdgeId,
    /// Named `generation` rather than `gen`: `gen` is a reserved keyword in
    /// edition 2024.
    pub generation: Generation,
    pub payload:    Value,
    /// The firing that emitted this token. Seed tokens use `FiringId(0)`.
    pub from:       FiringId,
}

impl Token {
    pub fn new(edge: EdgeId, generation: Generation, payload: Value, from: FiringId) -> Self {
        Self {
            edge,
            generation,
            payload,
            from,
        }
    }

    /// A token seeded onto an entry node or an expansion clone. `edge` is the
    /// synthetic seed edge the engine allocated for that node, so joins count
    /// it like any other incoming edge.
    pub fn seeded(edge: EdgeId, generation: Generation, payload: Value) -> Self {
        Self {
            edge,
            generation,
            payload,
            from: FiringId::new(0),
        }
    }
}

/// How a firing ended.
///
/// **This enum is closed.** These six variants are the complete and permanent
/// status vocabulary. Any future frontend concept must map onto them; none may
/// extend them. Attractor's first-class `RETRY` outcome, for instance, lowers
/// to `Failure` with `class: "retry_requested"` plus a matching `retry_on`, not
/// to a new variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Success,
    /// Soft failure or partial completion. Routing-visible, and success-like.
    ///
    /// `underlying` carries the real failure whenever one was converted into
    /// this, so the log never records a clean success for something that
    /// failed.
    PartialSuccess {
        underlying: Option<FailureInfo>,
    },
    Failure(FailureInfo),
    /// The precondition was false, or an upstream skip propagated.
    Skipped,
    Cancelled,
    TimedOut,
}

/// A [`Status`] with its payload stripped, for matching on the variant alone.
///
/// Deliberately unordered: no total order over these six means anything, and
/// declaration order is not one. Contrast
/// [`SplicePolicy`](crate::SplicePolicy), whose derived order *is* its
/// authority order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StatusKind {
    Success,
    PartialSuccess,
    Failure,
    Skipped,
    Cancelled,
    TimedOut,
}

/// The one place the status-to-kind mapping lives, so the two cannot drift.
impl From<&Status> for StatusKind {
    fn from(status: &Status) -> Self {
        match status {
            Status::Success => Self::Success,
            Status::PartialSuccess { .. } => Self::PartialSuccess,
            Status::Failure(_) => Self::Failure,
            Status::Skipped => Self::Skipped,
            Status::Cancelled => Self::Cancelled,
            Status::TimedOut => Self::TimedOut,
        }
    }
}

impl Status {
    /// The lowercase tag the status functions in expressions match on.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::PartialSuccess { .. } => "partial_success",
            Self::Failure(_) => "failure",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    /// The variant, without its payload. See [`From<&Status> for StatusKind`],
    /// which holds the only copy of this mapping.
    pub fn kind(&self) -> StatusKind {
        StatusKind::from(self)
    }

    /// **The** definition of success-likeness. Joins, cancel scopes, default
    /// success guards and retry all call this; none of them open-codes the
    /// match.
    ///
    /// A guard that needs to tell the two apart tests `partial_success()`
    /// explicitly.
    pub fn is_success_like(&self) -> bool {
        matches!(self, Self::Success | Self::PartialSuccess { .. })
    }

    /// Whether this status counts as a failure when folding the run status.
    /// `PartialSuccess`, `Skipped` and `Cancelled` do not.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failure(_) | Self::TimedOut)
    }

    /// The failure behind this status, if any: the failure itself, or the one a
    /// `PartialSuccess` was converted from.
    pub fn failure_info(&self) -> Option<&FailureInfo> {
        match self {
            Self::Failure(info) => Some(info),
            Self::PartialSuccess { underlying } => underlying.as_ref(),
            _ => None,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self::Failure(FailureInfo::new(message))
    }

    /// A soft failure that keeps the real failure on the record.
    pub fn partial(underlying: FailureInfo) -> Self {
        Self::PartialSuccess {
            underlying: Some(underlying),
        }
    }

    /// A partial completion that was never a failure to begin with.
    pub fn partial_clean() -> Self {
        Self::PartialSuccess { underlying: None }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureInfo {
    pub message: String,
    /// What kind of failure this is, for `retry_on` to match: `"network"`,
    /// `"rate_limit"`, `"exit_status:2"`, `"retry_requested"`. Step kinds set
    /// it. Empty means unclassified.
    #[serde(default)]
    pub class:   SmolStr,
}

impl FailureInfo {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            class:   SmolStr::default(),
        }
    }

    #[must_use]
    pub fn with_class(mut self, class: &str) -> Self {
        self.class = SmolStr::new(class);
        self
    }

    /// The conventional class for a process step that exited non-zero.
    pub fn exit_status(code: i32) -> Self {
        Self::new(format!("step exited with status {code}"))
            .with_class(&format!("exit_status:{code}"))
    }
}

/// Counters reported by a step. Free-form so step kinds can add their own.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code:   Option<i32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom:      BTreeMap<SmolStr, Value>,
}

impl Metrics {
    #[must_use]
    pub fn with_duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    #[must_use]
    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }
}

/// The result of one attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub status:          Status,
    /// Structured output: the value guards and `map` expressions see.
    pub output:          Value,
    #[serde(default)]
    pub metrics:         Metrics,
    /// Writes into [`RunContext::kv`], merged in event order by the core. This
    /// is the only path that writes run-scoped key/value state.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context_updates: BTreeMap<SmolStr, Value>,
    /// Ordered splice requests. Only a firing's **final** attempt applies them,
    /// in one transaction: every request prepares or none applies, and a
    /// rejection converts the whole outcome to `Failure{class:
    /// invalid_splice}`. A non-final attempt's requests are recorded in the
    /// event log and change nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub splices:         Vec<SpliceRequest>,
}

impl Outcome {
    pub fn new(status: Status, output: Value) -> Self {
        Self {
            status,
            output,
            metrics: Metrics::default(),
            context_updates: BTreeMap::new(),
            splices: Vec::new(),
        }
    }

    pub fn success(output: impl Into<Value>) -> Self {
        Self::new(Status::Success, output.into())
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self::new(Status::failure(message), Value::Null)
    }

    /// A soft failure. The real failure stays on the record.
    pub fn partial(underlying: FailureInfo, output: impl Into<Value>) -> Self {
        Self::new(Status::partial(underlying), output.into())
    }

    pub fn skipped() -> Self {
        Self::new(Status::Skipped, Value::Null)
    }

    pub fn cancelled() -> Self {
        Self::new(Status::Cancelled, Value::Null)
    }

    #[must_use]
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = metrics;
        self
    }

    #[must_use]
    pub fn with_context_update(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.context_updates.insert(SmolStr::new(key), value.into());
        self
    }

    /// Append one splice request. Order is preserved: a later request may
    /// reference a node an earlier one added.
    #[must_use]
    pub fn with_splice(mut self, request: SpliceRequest) -> Self {
        self.splices.push(request);
        self
    }

    #[must_use]
    pub fn with_splices(mut self, requests: Vec<SpliceRequest>) -> Self {
        self.splices = requests;
        self
    }
}

// ── Run context ───────────────────────────────────────────────────────────

/// What one node instance left behind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    /// The final attempt's status, raw.
    pub status:     Status,
    pub output:     Value,
    /// The latest generation to complete.
    pub generation: Generation,
    /// How many attempts the final firing took.
    pub attempts:   u32,
}

impl NodeRecord {
    /// The shape expressions see under `nodes.<id>`.
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "status": self.status.tag(),
            "output": self.output.clone(),
            "generation": self.generation.raw(),
            "attempts": self.attempts,
            "success_like": self.status.is_success_like(),
        })
    }
}

/// Run-scoped state that expressions can read.
///
/// Written **only** inside `apply`, in event order: node records when a
/// firing's final attempt finishes, and `kv` merged from
/// `Outcome::context_updates` in that same order, last write winning. No other
/// write path exists, which is what keeps the core pure and replay
/// byte-identical.
///
/// **`kv` merges on final attempts only**, like the node records. Guards read
/// `kv` concurrently — a parallel node's routing can consult it mid-run — so
/// merging a retried attempt's writes would let work that was later discarded
/// steer routing elsewhere in the graph. The invariant is that retries are
/// invisible everywhere except the event log. Per-attempt data is not lost:
/// every attempt's finish record carries its full outcome, `context_updates`
/// included, so tooling reads it from the log. It simply never enters the
/// routing-visible store.
///
/// This is derived state — reconstructible from the event log. It is part of
/// the engine state, but it is never checkpointed as a separate artifact.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunContext {
    /// Keyed by node instance name, so a matrix clone records under `build#2`.
    pub nodes: BTreeMap<SmolStr, NodeRecord>,
    pub kv:    BTreeMap<SmolStr, Value>,
}

impl RunContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn node(&self, name: &str) -> Option<&NodeRecord> {
        self.nodes.get(name)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.kv.get(key)
    }

    /// Record a completed firing. Called by the core only, on a final attempt.
    pub fn record(&mut self, name: SmolStr, record: NodeRecord) {
        self.nodes.insert(name, record);
    }

    /// Merge `context_updates`, last write winning. Called by the core only.
    pub fn merge(&mut self, updates: &BTreeMap<SmolStr, Value>) {
        for (key, value) in updates {
            self.kv.insert(key.clone(), value.clone());
        }
    }

    /// The `nodes` map as expressions see it.
    pub fn nodes_value(&self) -> Value {
        Value::Object(
            self.nodes
                .iter()
                .map(|(name, record)| (name.to_string(), record.to_value()))
                .collect(),
        )
    }

    /// The `kv` map as expressions see it.
    pub fn kv_value(&self) -> Value {
        Value::Object(
            self.kv
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
        )
    }
}

// ── Step progress and control ─────────────────────────────────────────────

/// Progress reported by a running step. Carries no coordination meaning.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StepEvent {
    Log { stream: LogStream, line: String },
    Artifact { name: SmolStr, uri: String },
    Custom(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// A signal delivered to a live firing.
///
/// Reserved seam: `Pause` lands here in v2 and reuses the same cancel-scope
/// machinery, so this enum is non-exhaustive from day one. `Steer` and
/// `Approve` shipped as [`Control::Deliver`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Control {
    /// Ask the step to stop: the polite ladder (TERM, grace, KILL).
    Cancel,
    /// Stop the step now: straight to `SIGKILL`, no ladder, no grace. Delivered
    /// by a `KillRequested` — to every live firing in the killed closure,
    /// ones already politely cancelling included, which a plain `Cancel`
    /// cannot say.
    Kill,
    /// A value delivered to a waiting step: a human's answer, a supervisor's
    /// instruction. May be delivered repeatedly to one firing (steering is a
    /// stream). Never starts the cancellation ladder or the kill tier.
    Deliver(Value),
}

/// How a whole run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Success,
    Failed,
    Cancelled,
}

/// The text a reader sees, in the same lowercase shape as [`Status::tag`], so
/// a run and its steps read the same way. `Debug` stays for diagnostics.
impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        })
    }
}
