//! The host-facing record stream.
//!
//! An [`EventObserver`] is the seam every host consumer hangs off — a run
//! store's ingest, a progress UI, a checkpoint writer, a watchdog. The driver
//! calls it after every `apply` with each newly appended record and the
//! post-apply state, so a consumer resolves a firing to its node, name and
//! `meta` in place ([`EngineState::firing_node`]) instead of keeping a
//! projection of its own.
//!
//! This is deliberately a callback and not a broadcast channel: broadcast drops
//! on lag, and a store ingest must never lose a record. Losslessness downstream
//! is the observer's job — hand slow work to a channel and return fast.

use std::error::Error;

use engine::{EngineState, EventRecord};

/// A sink for a run's record stream, registered on the driver before `run()`.
#[async_trait::async_trait]
pub trait EventObserver: Send + Sync {
    /// Every appended record, External and Core, in seq order, exactly once per
    /// driver lifetime — at-least-once across a resume, deduped by
    /// `(log identity, seq)`: `seq` is unique only within one log, and the
    /// log's identity is host-named (a run dir, a store key), stable across
    /// resume, fresh per fork.
    ///
    /// `state` is the post-apply engine state: resolve a firing to its node,
    /// name and `meta` here; borrow, don't keep. Called on the driver task —
    /// return fast, hand slow work to a channel. Infallible by design: the
    /// driver loop cannot meaningfully handle a sink error mid-apply, so an
    /// observer records its own failure and reports it from
    /// [`EventObserver::finish`].
    fn on_record(&self, record: &EventRecord, state: &EngineState);

    /// Awaited by `Driver::run` after the last record, before the report: drain
    /// queues, flush files, report what failed. Failures land in
    /// `RunReport::observer_errors` and never change the run status — a host
    /// with fatal-sink semantics watches its own observer and cancels via
    /// `RunHandle`.
    async fn finish(&self) -> Result<(), ObserveError> {
        Ok(())
    }
}

/// What an observer failed to do, reported from [`EventObserver::finish`].
#[derive(Debug, thiserror::Error)]
#[error("{observer}: {message}")]
pub struct ObserveError {
    /// Which observer failed, in the observer's own words (`events.jsonl`,
    /// say).
    pub observer: String,
    pub message:  String,
    /// The underlying failure, when the observer has a typed one to keep.
    #[source]
    pub source:   Option<Box<dyn Error + Send + Sync>>,
}

impl ObserveError {
    pub fn new(observer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            observer: observer.into(),
            message:  message.into(),
            source:   None,
        }
    }

    #[must_use]
    pub fn with_source(mut self, source: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        self.source = Some(source.into());
        self
    }
}
