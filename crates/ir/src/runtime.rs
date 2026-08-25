//! Values that flow while a graph runs: tokens, outcomes, step events, control signals.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use crate::ids::{EdgeId, FiringId, Generation};

/// One unit of flow, sitting on an edge and waiting for a join.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub edge: EdgeId,
    /// Named `generation` rather than `gen`: `gen` is a reserved keyword in
    /// edition 2024.
    pub generation: Generation,
    pub payload: Value,
    /// The firing that emitted this token. Seed tokens use `FiringId(0)`.
    pub from: FiringId,
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
    /// synthetic seed edge the engine allocated for that node, so joins count it
    /// like any other incoming edge.
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Success,
    Failure(FailureInfo),
    /// The precondition was false, or an upstream skip propagated.
    Skipped,
    Cancelled,
    TimedOut,
}

impl Status {
    /// The lowercase tag the `success()` / `failure()` expression functions match on.
    pub fn tag(&self) -> &'static str {
        match self {
            Status::Success => "success",
            Status::Failure(_) => "failure",
            Status::Skipped => "skipped",
            Status::Cancelled => "cancelled",
            Status::TimedOut => "timed_out",
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Status::Success)
    }

    /// Whether this status counts as a failure when folding the run status.
    /// `Skipped` and `Cancelled` do not.
    pub fn is_failure(&self) -> bool {
        matches!(self, Status::Failure(_) | Status::TimedOut)
    }

    pub fn failure(message: impl Into<String>) -> Status {
        Status::Failure(FailureInfo::new(message))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureInfo {
    pub message: String,
    /// Step-defined code, e.g. an exit status or an error class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<SmolStr>,
    /// A hint for retry policies; the core does not act on it in v1.
    #[serde(default)]
    pub retryable: bool,
}

impl FailureInfo {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            retryable: false,
        }
    }

    pub fn with_code(mut self, code: &str) -> Self {
        self.code = Some(SmolStr::new(code));
        self
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
}

/// Counters reported by a step. Free-form so step kinds can add their own.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom: BTreeMap<SmolStr, Value>,
}

impl Metrics {
    pub fn with_duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }
}

/// The result of one firing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub status: Status,
    /// Structured output: the value guards and `map` expressions see.
    pub output: Value,
    #[serde(default)]
    pub metrics: Metrics,
}

impl Outcome {
    pub fn new(status: Status, output: Value) -> Self {
        Self {
            status,
            output,
            metrics: Metrics::default(),
        }
    }

    pub fn success(output: impl Into<Value>) -> Self {
        Self::new(Status::Success, output.into())
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self::new(Status::failure(message), Value::Null)
    }

    pub fn skipped() -> Self {
        Self::new(Status::Skipped, Value::Null)
    }

    pub fn cancelled() -> Self {
        Self::new(Status::Cancelled, Value::Null)
    }

    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = metrics;
        self
    }
}

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
/// Reserved seam: `Pause` / `Steer` / `Approve` land here in v2 and reuse the same
/// cancel-scope machinery, so this enum is non-exhaustive from day one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Control {
    Cancel,
}

/// How a whole run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Success,
    Failed,
    Cancelled,
}
