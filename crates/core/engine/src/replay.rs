//! Replay: rebuild a run from its event log. Resume: rebuild it and say what
//! is still owed.
//!
//! Only [`EventSource::External`] records are fed back. Everything the core
//! produced — routed tokens, splices, cascading cancellations — it produces
//! again. That is what makes a byte-identical replayed log a real determinism
//! check rather than a copy: if any core decision depended on a clock, on
//! iteration order, or on anything outside the state, the two logs diverge.
//!
//! [`resume`] leans on the same determinism from the other side: because
//! `apply` regenerates every command byte-identically, nothing about in-flight
//! work needs separate persistence — the log alone says which effects a dead
//! driver still owed.

use std::collections::BTreeMap;

use ir::{FiringId, Graph};

use crate::apply::apply;
use crate::event::Command;
use crate::log::EventLog;
use crate::state::EngineState;

/// Replay a log against the graph the run started from.
///
/// The graph must be the original one, before any splice: expansions are
/// re-derived from the events, not read back from a mutated graph.
pub fn replay(graph: Graph, log: &EventLog) -> EngineState {
    replay_with(graph, log, drop)
}

/// The replay fold: every external event applied in order, each step's commands
/// handed to `on_commands`.
fn replay_with(
    graph: Graph,
    log: &EventLog,
    mut on_commands: impl FnMut(Vec<Command>),
) -> EngineState {
    let mut state = EngineState::new(graph);
    for event in log.external_events() {
        let (next, commands) = apply(state, event.clone());
        state = next;
        on_commands(commands);
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

/// Everything a driver needs to continue a run whose process died.
pub struct ResumePoint {
    /// The rebuilt state. Its log is the loaded prefix plus every record replay
    /// regenerated past it.
    pub state:        EngineState,
    /// The effects still owed, in dispatch order: `AcquireScope` for each held
    /// scope, then — per live firing — either its last `StartStep` (not
    /// awaiting a retry) or its last `ScheduleRetry` (awaiting one). The
    /// two sets are disjoint because `awaiting_retry()` is a subset of
    /// `live_firings()`, and re-issuing both would execute the step before
    /// its backoff.
    pub pending:      Vec<Command>,
    /// The firings whose step will actually execute again — live, not awaiting
    /// a retry, not cancelling. Resume is invisible in the log, so this is
    /// where a host learns what to mint new execution identities for.
    pub redispatched: Vec<FiringId>,
}

/// Rebuild a crashed run's state by replay and reconcile what is still owed.
///
/// The loaded log must be a **byte-prefix** of the regenerated one — not equal:
/// a crash can land between an External append and the flush of the Core
/// records it derived, so the regenerated log may be longer, and that is fine.
/// Anything else is real divergence and refuses with [`ReplayMismatch`].
///
/// Deliberately absent from `pending`: `DeliverControl` — a logged
/// `ControlRequested{Deliver}` made the state reproducible, but the payload is
/// not re-forwarded; the resumed step waits again and the host re-sends what
/// its own store says is outstanding.
pub fn resume(graph: Graph, log: &EventLog) -> Result<ResumePoint, ReplayMismatch> {
    let mut last_start: BTreeMap<FiringId, Command> = BTreeMap::new();
    let mut last_retry: BTreeMap<FiringId, Command> = BTreeMap::new();
    let state = replay_with(graph, log, |commands| {
        for command in commands {
            match &command {
                Command::StartStep(resolved) => {
                    last_start.insert(resolved.id(), command);
                }
                Command::ScheduleRetry { firing, .. } => {
                    last_retry.insert(*firing, command);
                }
                // Everything else is reconciled from the final state (scopes)
                // or deliberately not re-issued (deliveries).
                Command::DeliverControl { .. }
                | Command::Admit { .. }
                | Command::ResolveRouting { .. }
                | Command::ExpandNode { .. }
                | Command::AcquireScope { .. }
                | Command::ReleaseScope { .. }
                | Command::FinishRun { .. }
                | Command::FinishExecution { .. } => {}
            }
        }
    });
    verify_prefix(log, &state.log)?;

    let mut pending: Vec<Command> = state
        .held_scopes()
        .map(|scope| Command::AcquireScope { scope })
        .collect();
    pending.extend(state.pending_admissions().map(|admission| Command::Admit {
        point:       admission.point,
        decision_id: admission.decision_id,
    }));
    pending.extend(
        state
            .pending_routings()
            .map(|routing| Command::ResolveRouting {
                firing:          routing.firing,
                decision_id:     routing.decision_id,
                restart_allowed: routing.restart_allowed,
                groups:          routing.groups.clone(),
            }),
    );
    let mut redispatched = Vec::new();
    for firing in state.live_firings() {
        if firing.awaiting_admission {
            continue;
        }
        if firing.awaiting_retry {
            let retry = last_retry
                .get(&firing.id)
                .cloned()
                .expect("an awaiting-retry firing has a ScheduleRetry; apply is deterministic");
            pending.push(retry);
        } else {
            let start = last_start
                .get(&firing.id)
                .cloned()
                .expect("a live firing has a StartStep; apply is deterministic");
            if !firing.cancelling {
                redispatched.push(firing.id);
            }
            pending.push(start);
        }
    }
    Ok(ResumePoint {
        state,
        pending,
        redispatched,
    })
}

/// The loaded log must be a byte-prefix of the regenerated one. Byte-wise per
/// record — record `PartialEq` is weaker (JSON maps compare
/// order-insensitively). `verify_replay` is this plus "and no longer".
fn verify_prefix(loaded: &EventLog, rebuilt: &EventLog) -> Result<(), ReplayMismatch> {
    let mismatch = |first_divergence| ReplayMismatch {
        original_records: loaded.len(),
        replayed_records: rebuilt.len(),
        first_divergence,
    };
    for (a, b) in loaded.records().iter().zip(rebuilt.records()) {
        let original = serde_json::to_vec(a).expect("a record always encodes");
        let regenerated = serde_json::to_vec(b).expect("a record always encodes");
        if original != regenerated {
            return Err(mismatch(Some(a.seq)));
        }
    }
    if rebuilt.len() < loaded.len() {
        return Err(mismatch(None));
    }
    Ok(())
}

/// Replay a log and check that the result is byte-identical to the original.
///
/// This is the determinism canary. Run it wherever a new source of state enters
/// the core.
pub fn verify_replay(graph: Graph, log: &EventLog) -> Result<EngineState, ReplayMismatch> {
    let state = replay(graph, log);
    verify_prefix(log, &state.log)?;
    if state.log.len() > log.len() {
        return Err(ReplayMismatch {
            original_records: log.len(),
            replayed_records: state.log.len(),
            first_divergence: None,
        });
    }
    Ok(state)
}
