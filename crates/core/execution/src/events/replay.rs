//! Replay from a run's store: the whole run at once ([`replay_run`]) or
//! incrementally between reads ([`RunReplay`]).

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::mem;
use std::path::Path;

use engine::{EngineState, EventOrigin, EventRecord};
use ir::Graph;
use store::{Access, LogId, RunLogs};

use super::{EventId, EventSource, Projection, RunEvent};
use crate::store::{decode_coordinator_record, decode_graph, graph_bytes, read_coordinator_log};
use crate::{
    CoordinatorRecord, CoordinatorState, DecodedEngineLog, EngineLogDecodeError, EngineLogError,
    ExecutionId, GraphDigest, StateError, StoreError, StoredEngineRecord, open_run_dir,
    read_execution_log,
};

/// Why a run could not be projected from its store.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    EngineLog(#[from] EngineLogError),
}

impl From<store::StoreError> for ReplayError {
    fn from(error: store::StoreError) -> Self {
        Self::Store(error.into())
    }
}

/// Project every event of a run from its store: the coordinator log first,
/// then each execution's engine log in declaration order, each replayed
/// external event by external event so the derivation sees the same
/// post-apply states the live observer saw. Identities equal the live ones.
pub async fn replay_run(logs: &dyn RunLogs) -> Result<Vec<RunEvent>, ReplayError> {
    let mut projection = Projection::new();
    project_run(logs, &mut projection).await
}

/// [`replay_run`] over the run directory at `run_dir`.
pub async fn replay_run_dir(run_dir: &Path) -> Result<Vec<RunEvent>, ReplayError> {
    let logs = open_run_dir(run_dir, Access::Read).await?;
    replay_run(&*logs).await
}

/// The events after a set of per-log positions: the incremental form of
/// [`replay_run`], for a consumer that already holds a prefix of the stream
/// and asks for the rest. `held` names the last [`EventId`] the consumer
/// has from each log; a log it names nothing for is replayed whole. The
/// fold still runs over the whole run, since a suffix cannot be derived
/// without the state the prefix built; only the delivery is trimmed.
pub async fn replay_since(
    logs: &dyn RunLogs,
    held: &BTreeMap<EventSource, EventId>,
) -> Result<Vec<RunEvent>, ReplayError> {
    let events = replay_run(logs).await?;
    Ok(events_after(events, held))
}

/// Keep the events past each log's held position.
pub(crate) fn events_after(
    events: Vec<RunEvent>,
    held: &BTreeMap<EventSource, EventId>,
) -> Vec<RunEvent> {
    events
        .into_iter()
        .filter(|event| {
            held.get(&event.id.source)
                .is_none_or(|last| event.id > *last)
        })
        .collect()
}

/// [`replay_run`] through a caller's projection state.
pub(super) async fn project_run(
    logs: &dyn RunLogs,
    projection: &mut Projection,
) -> Result<Vec<RunEvent>, ReplayError> {
    Ok(project_loaded(&load_run(logs).await?, projection))
}

/// Project a loaded run: the coordinator log first, then each execution's
/// engine log in declaration order.
pub(crate) fn project_loaded(loaded: &LoadedRun, projection: &mut Projection) -> Vec<RunEvent> {
    let mut events = Vec::new();
    for record in &loaded.coordinator {
        events.extend(projection.lifecycle(record));
    }
    for execution in &loaded.executions {
        events.extend(replay_execution(
            projection,
            execution.execution,
            execution.graph.clone(),
            &execution.log.log,
            &execution.log.recorded_at,
        ));
    }
    events
}

/// One run's durable logs as its store holds them: the coordinator log and,
/// for each execution in declaration order that started, its graph and its
/// engine log.
pub(crate) struct LoadedRun {
    pub(crate) coordinator: Vec<CoordinatorRecord>,
    pub(crate) executions:  Vec<LoadedExecution>,
}

pub(crate) struct LoadedExecution {
    pub(crate) execution: ExecutionId,
    /// The graph the execution started from, before any splice.
    pub(crate) graph:     Graph,
    pub(crate) log:       DecodedEngineLog,
    /// Whether the coordinator recorded the execution's exit: a finished
    /// execution's log is complete, or the crash that cut it is the error.
    pub(crate) finished:  bool,
}

/// Read a run's logs and the graphs its executions ran.
pub(crate) async fn load_run(logs: &dyn RunLogs) -> Result<LoadedRun, ReplayError> {
    let coordinator = read_coordinator_log(logs).await?;
    let state = CoordinatorState::replay(&coordinator)?;
    let mut graphs: BTreeMap<crate::GraphDigest, Graph> = BTreeMap::new();
    let mut executions = Vec::new();
    for (execution, declared) in &state.executions {
        let invocation = declared.declaration.invocation;
        let digest = state.invocations[&invocation].declaration.graph;
        if let Entry::Vacant(entry) = graphs.entry(digest) {
            let bytes = graph_bytes(logs, digest).await?;
            entry.insert(decode_graph(digest, &bytes)?);
        }
        let log = read_execution_log(logs, *execution).await?;
        if log.log.is_empty() {
            // Declared, and nothing stored yet: the execution never started.
            continue;
        }
        executions.push(LoadedExecution {
            execution: *execution,
            graph: graphs[&digest].clone(),
            log,
            finished: declared.exit.is_some(),
        });
    }
    Ok(LoadedRun {
        coordinator,
        executions,
    })
}

/// Project one execution's log through `projection`, external event by
/// external event. `recorded_at` is the log's recording time per seq. Only
/// the stored prefix is published: a regenerated record past the log's end
/// (a crash's lost tail) is applied, so the state is right, and derives no
/// event until a resume stores it.
pub fn replay_execution(
    projection: &mut Projection,
    execution: ExecutionId,
    graph: Graph,
    log: &engine::EventLog,
    recorded_at: &[u64],
) -> Vec<RunEvent> {
    let mut state = EngineState::new(graph);
    let mut events = Vec::new();
    for event in log.external_events() {
        let before = state.log.len();
        let (next, _) = engine::apply(state, event.clone());
        state = next;
        for record in &state.log.records()[before..] {
            let stored = usize::try_from(record.seq)
                .ok()
                .filter(|seq| *seq < log.len())
                .and_then(|seq| recorded_at.get(seq).copied());
            if let Some(at) = stored {
                events.extend(projection.engine(execution, record, at, &state));
            }
        }
    }
    events
}

// ── Incremental replay ─────────────────────────────────────────────────

/// A run's replay kept between reads: the form of [`replay_run`] for a
/// consumer that follows a live run through its store. Each
/// [`advance`](Self::advance) reads what the store holds past the records
/// the replay consumed, folds them through the same derivation, and hands
/// back the events of the new records alone, with the identities the full
/// replay gives them. What it keeps is what the derivation needs and the
/// full replay rebuilds on every call: the coordinator state, each
/// execution's engine state with its log, and the [`Projection`].
///
/// Per log, the events of a replay advanced over a run in any number of
/// reads are the events of [`replay_run`], in the same order; across logs
/// only the interleaving differs, since an advance hands back each log's
/// new events together (the coordinator log first, then each execution in
/// declaration order, as the full replay does). A record a crash left an
/// execution's log short of is applied, so the state is right, and derives
/// no event until a later advance finds it stored, as under the full
/// replay; the records of an execution the coordinator log has not
/// declared yet wait for the declaration.
///
/// The state is a cache of the records: a fresh replay rebuilds it, and
/// it holds every engine record of the run in memory, so a consumer drops
/// it when the run ends or goes idle. An advance reads everything before
/// it folds anything, so an advance that fails leaves the replay as it
/// was, to be retried once the store is whole.
#[derive(Default)]
pub struct RunReplay {
    projection:  Projection,
    coordinator: CoordinatorState,
    /// Coordinator records consumed: the seq the next read starts at.
    consumed:    usize,
    /// The graphs read so far, so a second execution of one graph reads no
    /// blob.
    graphs:      BTreeMap<GraphDigest, Graph>,
    executions:  BTreeMap<ExecutionId, ExecutionReplay>,
}

/// One execution's replay: its engine state, and how far into its stored
/// log the events were published.
struct ExecutionReplay {
    state:     EngineState,
    /// Stored records whose events were published: every seq below it. The
    /// state's log may run past it, by the records the last external apply
    /// regenerated before the log held them.
    published: usize,
}

/// What one advance read of an execution's log, before the fold.
struct ExecutionTail {
    execution: ExecutionId,
    /// The graph the execution started from, read for an execution seen
    /// for the first time.
    start:     Option<(GraphDigest, Graph)>,
    records:   Vec<(EventRecord, u64)>,
}

impl RunReplay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the records past the ones consumed, fold them, and hand back
    /// their events: the coordinator log's first, then each execution's in
    /// declaration order. Empty when the store holds nothing new.
    ///
    /// # Errors
    ///
    /// What [`replay_run`] fails with over the same store: a log that does
    /// not read or decode, a coordinator record the state rejects, a gap in
    /// a log (a torn tail). The replay is unchanged by a failed advance.
    pub async fn advance(&mut self, logs: &dyn RunLogs) -> Result<Vec<RunEvent>, ReplayError> {
        // Every read and decode happens here, before the fold below changes
        // anything, so a failure leaves the replay where it stood.
        let coordinator = self.read_coordinator(logs).await?;
        let mut state = self.coordinator.clone();
        for (offset, record) in coordinator.iter().enumerate() {
            let index = self.consumed + offset;
            if record.seq != index as u64 {
                return Err(StateError::Sequence {
                    index,
                    found: record.seq,
                }
                .into());
            }
            state.apply(&record.body)?;
        }
        if state.root.is_none() {
            return Err(StateError::MissingRunStart.into());
        }
        let mut tails = Vec::new();
        for (execution, declared) in &state.executions {
            let replay = self.executions.get(execution);
            let published = replay.map_or(0, |replay| replay.published);
            let stored = logs
                .read_from(&LogId::Execution(*execution), published as u64)
                .await?;
            if stored.is_empty() {
                continue;
            }
            let records = decode_engine_tail(*execution, published, &stored)?;
            let start = if replay.is_some() {
                None
            } else {
                let digest = state.invocations[&declared.declaration.invocation]
                    .declaration
                    .graph;
                let graph = match self.graphs.get(&digest) {
                    Some(graph) => graph.clone(),
                    None => decode_graph(digest, &graph_bytes(logs, digest).await?)?,
                };
                Some((digest, graph))
            };
            tails.push(ExecutionTail {
                execution: *execution,
                start,
                records,
            });
        }

        let mut events = Vec::new();
        for record in &coordinator {
            events.extend(self.projection.lifecycle(record));
        }
        self.coordinator = state;
        self.consumed += coordinator.len();
        for tail in tails {
            let replay = match self.executions.entry(tail.execution) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let Some((digest, graph)) = tail.start else {
                        continue;
                    };
                    self.graphs.entry(digest).or_insert_with(|| graph.clone());
                    entry.insert(ExecutionReplay {
                        state:     EngineState::new(graph),
                        published: 0,
                    })
                }
            };
            events.extend(replay.advance(&mut self.projection, tail.execution, tail.records));
        }
        Ok(events)
    }

    /// The coordinator records past the consumed ones, decoded under the
    /// rules of [`read_coordinator_log`]: the format check reads the log's
    /// first record, so the first read takes the log whole.
    async fn read_coordinator(
        &self,
        logs: &dyn RunLogs,
    ) -> Result<Vec<CoordinatorRecord>, ReplayError> {
        if self.consumed == 0 {
            return read_coordinator_log(logs).await.map_err(ReplayError::from);
        }
        let stored = logs
            .read_from(&LogId::Coordinator, self.consumed as u64)
            .await?;
        stored
            .iter()
            .map(|record| decode_coordinator_record(record).map_err(ReplayError::from))
            .collect()
    }
}

impl ExecutionReplay {
    /// Fold a tail of the execution's stored log, contiguous from
    /// `published`, and hand back the events of the records it holds, as
    /// [`replay_execution`] derives them: external event by external event
    /// with the post-apply state, each record published once the log holds
    /// it, with the recording time the log gives it.
    fn advance(
        &mut self,
        projection: &mut Projection,
        execution: ExecutionId,
        tail: Vec<(EventRecord, u64)>,
    ) -> Vec<RunEvent> {
        let published = self.published;
        let times: Vec<u64> = tail.iter().map(|(_, at)| *at).collect();
        let stored_time = |seq: u64| {
            usize::try_from(seq)
                .ok()
                .and_then(|seq| seq.checked_sub(published))
                .and_then(|index| times.get(index).copied())
        };
        let mut events = Vec::new();
        // The next seq to publish: an apply publishes the records it
        // produced as far as the log holds them, and the tail's own copies
        // of those records are then passed over.
        let mut next = published;
        for (record, recorded_at) in tail {
            let seq = usize::try_from(record.seq).unwrap_or(usize::MAX);
            if seq < next {
                continue;
            }
            if let Some(held) = self.state.log.records().get(seq) {
                // Regenerated by the last external apply before the log held
                // it: published now, with that apply's state, which no later
                // external event has moved.
                events.extend(projection.engine(execution, held, recorded_at, &self.state));
                next = seq + 1;
                continue;
            }
            if record.origin != EventOrigin::External {
                continue;
            }
            let before = self.state.log.len();
            let state = mem::replace(&mut self.state, EngineState::new(Graph::new()));
            let (applied, _) = engine::apply(state, record.event);
            self.state = applied;
            for produced in &self.state.log.records()[before..] {
                // A record past the stored log derives no event until a
                // later advance finds it stored.
                let Some(at) = stored_time(produced.seq) else {
                    continue;
                };
                events.extend(projection.engine(execution, produced, at, &self.state));
                next = usize::try_from(produced.seq).map_or(next, |seq| seq + 1);
            }
        }
        self.published = published + times.len();
        events
    }
}

/// Decode the stored tail of an execution's log, contiguous from `from`,
/// into engine records with their recording times, under the rules of
/// [`decode_engine_records`](crate::decode_engine_records): a line that
/// does not decode and a gap in the seqs are the errors the full decode
/// reports, at the same line numbers.
fn decode_engine_tail(
    execution: ExecutionId,
    from: usize,
    stored: &[store::Record],
) -> Result<Vec<(EventRecord, u64)>, ReplayError> {
    let decode_error = |source| EngineLogError::Decode { execution, source };
    let mut records = Vec::with_capacity(stored.len());
    for (offset, line) in stored.iter().enumerate() {
        let index = from + offset;
        let stored: StoredEngineRecord = line.decode().map_err(|source| {
            decode_error(EngineLogDecodeError::BadRecord {
                line: index + 1,
                source,
            })
        })?;
        if stored.seq != index as u64 {
            return Err(decode_error(
                engine::InvalidRecords::SeqMismatch {
                    index,
                    found: stored.seq,
                }
                .into(),
            )
            .into());
        }
        records.push(stored.into_parts());
    }
    Ok(records)
}
