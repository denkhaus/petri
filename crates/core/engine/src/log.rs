//! The event log. Versioned so replay and resume can rely on the format.

use serde::{Deserialize, Serialize};

use crate::event::Event;

/// Bumped whenever the shape of a record changes.
///
/// v1 → v2: the firing key gained [`ir::Attempt`], `StepStarted` / `StepFinished`
/// carry it, `ScheduleRetry` / `RetryElapsed` joined the vocabulary, finish records
/// carry `context_updates`, and every record records whether it came from outside or
/// from the core.
///
/// v2 → v3: cancelled outcomes route (the semantics change under replay),
/// `KillRequested` joined the vocabulary, and `Node` — serialized inside
/// `NodeExpanded` splices — gained `run_on_cancel`. Per the standing policy there is
/// no migrator: a v2 log is rejected cleanly.
pub const LOG_VERSION: u32 = 3;

/// Where an event came from.
///
/// **This enum is closed.** `External` and `Core` are the complete and permanent
/// vocabulary: an event either entered from outside the core or the core produced it,
/// and there is no third case. Nothing may extend it.
///
/// # The verification contract
///
/// This is load-bearing for every determinism claim the system makes, so it is worth
/// stating plainly:
///
/// - Replay feeds back **only** `External` records.
/// - Every `Core` record is **regenerated** by the core during replay, never replayed
///   from the log.
/// - A replayed log that is byte-identical to the original therefore asserts that the
///   core reached every one of those `Core` events again, in the same order, from the
///   same inputs.
///
/// The regenerated-versus-recorded distinction *is* the assertion. It is not
/// redundancy, and it is not an optimisation. If a future change feeds `Core` records
/// back instead of regenerating them, `verify_replay` keeps passing while asserting
/// nothing at all: it would be comparing the log against a copy of itself. Anything
/// that makes the core consult a clock, an RNG, an environment variable, or a
/// non-deterministic iteration order breaks byte-identity — which is the point.
///
/// A host feeding an event in is `External` even when that event describes something
/// the core asked for, such as `RetryElapsed` answering a `ScheduleRetry`: the
/// decision to send it, and when, came from outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventSource {
    /// Fed in by the host: run start, step results, retry timers, cancellation.
    External,
    /// Emitted by the core while draining: routed tokens, splices, cascades.
    Core,
}

/// Where a cancellation's escalation is recorded, and why it is not a failure class.
///
/// A consumer reading the log for "how did this step get stopped" looks here, not at
/// [`ir::Status`]. `Status::Cancelled` and `Status::TimedOut` carry no `FailureInfo`,
/// and giving them one would widen an enum the core declares closed and permanent.
/// So the escalation is a field on the finish record's `outcome.output`:
///
/// | Value | Meaning |
/// |---|---|
/// | `sigterm` | the step exited within the grace period after `SIGTERM` |
/// | `sigkill` | the grace period ran out and the group was killed |
/// | `cancel_forced` | the step kind never returned; the driver stopped waiting |
///
/// The key is absent on any outcome that was not cancelled or timed out.
pub const CANCEL_ESCALATION_KEY: &str = "cancel_escalation";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Position in the log, starting at 0.
    pub seq: u64,
    pub source: EventSource,
    pub event: Event,
}

/// A log whose version is not the one this build speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("event log is version {found}; this build reads version {expected}")]
pub struct UnsupportedLogVersion {
    pub found: u32,
    pub expected: u32,
}

/// An append-only list of every event the run has applied, in order.
///
/// The core appends here before applying, including for the events it emits itself
/// while routing. Replaying the external records through `apply` from a fresh state
/// reproduces the run exactly, because `apply` has no other inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "EventLogRepr", try_from = "EventLogRepr")]
pub struct EventLog {
    version: u32,
    records: Vec<EventRecord>,
}

impl Default for EventLog {
    fn default() -> Self {
        Self {
            version: LOG_VERSION,
            records: Vec::new(),
        }
    }
}

impl EventLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// The format version. Always [`LOG_VERSION`] for a log this build produced.
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn append(&mut self, source: EventSource, event: Event) -> u64 {
        let seq = self.records.len() as u64;
        self.records.push(EventRecord { seq, source, event });
        seq
    }

    pub fn records(&self) -> &[EventRecord] {
        &self.records
    }

    pub fn events(&self) -> impl Iterator<Item = &Event> {
        self.records.iter().map(|r| &r.event)
    }

    /// The events a host fed in, in order. This is what replay consumes.
    pub fn external_events(&self) -> impl Iterator<Item = &Event> {
        self.records
            .iter()
            .filter(|r| r.source == EventSource::External)
            .map(|r| &r.event)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// The wire shape. Reading one goes through the version check, so a v1 log is
/// rejected cleanly rather than half-understood.
#[derive(Serialize, Deserialize)]
struct EventLogRepr {
    version: u32,
    records: Vec<EventRecord>,
}

impl From<EventLog> for EventLogRepr {
    fn from(log: EventLog) -> Self {
        Self {
            version: log.version,
            records: log.records,
        }
    }
}

impl TryFrom<EventLogRepr> for EventLog {
    type Error = UnsupportedLogVersion;

    fn try_from(repr: EventLogRepr) -> Result<Self, Self::Error> {
        if repr.version != LOG_VERSION {
            return Err(UnsupportedLogVersion {
                found: repr.version,
                expected: LOG_VERSION,
            });
        }
        Ok(Self {
            version: repr.version,
            records: repr.records,
        })
    }
}
