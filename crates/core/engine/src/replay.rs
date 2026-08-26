//! Replay: rebuild a run from its event log.
//!
//! Only [`EventSource::External`] records are fed back. Everything the core produced
//! — routed tokens, splices, cascading cancellations — it produces again. That is
//! what makes a byte-identical replayed log a real determinism check rather than a
//! copy: if any core decision depended on a clock, on iteration order, or on
//! anything outside the state, the two logs diverge.

use ir::Graph;

use crate::apply::apply;
use crate::log::EventLog;
use crate::state::EngineState;

/// Replay a log against the graph the run started from.
///
/// The graph must be the original one, before any splice: expansions are re-derived
/// from the events, not read back from a mutated graph.
pub fn replay(graph: Graph, log: &EventLog) -> EngineState {
    let mut state = EngineState::new(graph);
    for event in log.external_events() {
        let (next, _commands) = apply(state, event.clone());
        state = next;
    }
    state
}

/// A replayed log that does not match the original, byte for byte.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "replay diverged from the original log: {original_records} records became \
     {replayed_records}{}",
    .first_divergence.map(|seq| format!(", first difference at seq {seq}")).unwrap_or_default()
)]
pub struct ReplayMismatch {
    pub original_records: usize,
    pub replayed_records: usize,
    /// The first sequence number whose record differs, when both logs reach it.
    pub first_divergence: Option<u64>,
}

/// Replay a log and check that the result is byte-identical to the original.
///
/// This is the determinism canary. Run it wherever a new source of state enters the
/// core.
pub fn verify_replay(graph: Graph, log: &EventLog) -> Result<EngineState, ReplayMismatch> {
    let state = replay(graph, log);
    let (original, replayed) = (log, &state.log);

    let encoded_original = serde_json::to_vec(original).expect("a log always encodes");
    let encoded_replayed = serde_json::to_vec(replayed).expect("a log always encodes");
    if encoded_original == encoded_replayed {
        return Ok(state);
    }

    let first_divergence = original
        .records()
        .iter()
        .zip(replayed.records())
        .find(|(a, b)| a != b)
        .map(|(a, _)| a.seq);
    Err(ReplayMismatch {
        original_records: original.len(),
        replayed_records: replayed.len(),
        first_divergence,
    })
}
