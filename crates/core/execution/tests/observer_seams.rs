//! The host-facing observer seams a store hangs off: an execution observer
//! takes part in the durability acknowledgement, replay can start from
//! held positions, and a replay kept between reads derives the same events
//! piece by piece.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use driver::ObserveError;
use engine::{EngineState, EventRecord};
use execution::events::{
    EventId, EventSource, ReplayError, RunEvent, RunReplay, replay_run, replay_since,
};
use execution::host::{self, HostRun};
use execution::{
    Access, CoordinatorEvent, CoordinatorRecord, ExecutionId, ExecutionObserver, LogId,
    MemoryRunStore, OwnerId, Record, RunKey, RunLogs, RunStore, read_coordinator_log,
};
use ir::{GraphBuilder, JoinPolicy, Outcome, RunStatus, ScopeId, StepEvent};
use runtime::steps::{ProgressError, Registry, StepCtx, StepRunner};
use runtime::{RunOptions, Runtime};
use serde_json::json;
use store::{Digest, StoreError};
use testkit::{RunDir, add_script};

/// An observer whose storage never confirms a record.
struct NeverDurable;

#[async_trait::async_trait]
impl ExecutionObserver for NeverDurable {
    fn on_engine_record(&self, _: ExecutionId, _: &EventRecord, _: u64, _: &EngineState) {}

    fn on_lifecycle(&self, _: &CoordinatorRecord) {}

    async fn durable(&self, execution: ExecutionId, seq: u64) -> Result<(), ObserveError> {
        Err(ObserveError::new(
            "never-durable",
            format!("execution {execution} record {seq} was not stored"),
        ))
    }
}

const ACKED: ir::StepKindId = ir::StepKindId::new_static("acked");

/// Sends one acknowledged event and reports what the acknowledgement said.
struct AckedStep;

impl ir::StepKind for AckedStep {
    fn id(&self) -> ir::StepKindId {
        ACKED
    }

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "acked"
    }
}

#[async_trait::async_trait]
impl StepRunner for AckedStep {
    async fn run(&self, ctx: StepCtx) -> Outcome {
        let acknowledged = ctx
            .logs
            .send_acked(StepEvent::Custom(json!({ "kind": "seam-test" })))
            .await;
        match acknowledged {
            Ok(()) => Outcome::success(json!("durable")),
            Err(ProgressError::NotDurable { source }) => {
                Outcome::success(json!({ "not_durable": source.to_string() }))
            }
            Err(other) => Outcome::success(json!({ "other": other.to_string() })),
        }
    }
}

/// An execution observer that refuses `durable` reaches the step as
/// `ProgressError::NotDurable`: the addressed observer forwards the
/// acknowledgement, not just the record.
#[tokio::test]
async fn an_execution_observer_that_fails_durable_reaches_the_step_as_not_durable() {
    let dir = RunDir::new("observer-not-durable");
    let mut registry = Registry::new();
    registry.register_runner(Arc::new(AckedStep));
    let rt = Runtime::standard()
        .steps(registry)
        .options(RunOptions::new(dir.path()));
    let mut b = GraphBuilder::new();
    b.add_step("work", ScopeId::new(0), ACKED);
    let report = host::run_configured(
        &rt,
        HostRun::new(b.build()).observe(Arc::new(NeverDurable)),
        |_, _| {},
    )
    .await
    .expect("the run completes");
    assert_eq!(report.status, RunStatus::Success);
    let output = testkit::output_of(&report, "work");
    let message = output["not_durable"]
        .as_str()
        .expect("the step heard `NotDurable`: {output}");
    assert!(message.contains("never-durable"), "{message}");
}

/// Replay from held positions is the tail of the full replay: every event
/// past each log's held id, none before it, and nothing from a log the
/// caller holds no position for is dropped.
#[tokio::test]
async fn replay_from_held_positions_is_the_tail_of_the_full_replay() {
    let dir = RunDir::new("replay-since");
    let rt = Runtime::standard().options(RunOptions::new(dir.path()));
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let first = add_script(&mut b, "first", scope, "echo one");
    let second = add_script(&mut b, "second", scope, "echo two");
    b.link(first, second);
    let report = host::run(&rt, b.build()).await.expect("the run completes");
    assert_eq!(report.status, RunStatus::Success);

    let logs = testkit::read_run_dir(dir.path()).await;
    let full = replay_run(&*logs).await.expect("the run projects");
    let execution = full
        .iter()
        .map(|event| event.id.source)
        .find(|source| matches!(source, EventSource::Execution { .. }))
        .expect("an execution log");
    let engine: Vec<&EventId> = full
        .iter()
        .filter(|event| event.id.source == execution)
        .map(|event| &event.id)
        .collect();
    assert!(engine.len() > 4, "enough engine events to cut: {engine:?}");
    let held_engine = *engine[engine.len() / 2];
    let coordinator_last = full
        .iter()
        .filter(|event| event.id.source == EventSource::Coordinator)
        .map(|event| event.id)
        .max()
        .expect("coordinator events");

    // Hold the engine log at its midpoint and the coordinator log whole.
    let mut held = BTreeMap::new();
    held.insert(execution, held_engine);
    held.insert(EventSource::Coordinator, coordinator_last);
    let tail = replay_since(&*logs, &held)
        .await
        .expect("the suffix projects");
    let expected: Vec<_> = full
        .iter()
        .filter(|event| event.id.source == execution && event.id > held_engine)
        .cloned()
        .collect();
    assert_eq!(tail, expected, "the tail past the held engine position");
    assert!(
        tail.iter()
            .all(|event| event.id.source != EventSource::Coordinator),
        "a log held whole contributes nothing"
    );

    // A log with no held position is replayed whole.
    let mut held = BTreeMap::new();
    held.insert(execution, held_engine);
    let tail = replay_since(&*logs, &held)
        .await
        .expect("the suffix projects");
    let coordinator: Vec<_> = tail
        .iter()
        .filter(|event| event.id.source == EventSource::Coordinator)
        .collect();
    assert_eq!(
        coordinator.len(),
        full.iter()
            .filter(|event| event.id.source == EventSource::Coordinator)
            .count()
    );

    // Nothing held: the full replay.
    assert_eq!(
        replay_since(&*logs, &BTreeMap::new())
            .await
            .expect("projects"),
        full
    );
}

/// The logs of a run as its store holds them, in the order a consumer
/// reads them: the coordinator log, then each execution's log.
async fn stored_logs(logs: &dyn RunLogs, events: &[RunEvent]) -> Vec<(LogId, Vec<Record>)> {
    let mut ids = vec![LogId::Coordinator];
    let executions: BTreeSet<ExecutionId> = events
        .iter()
        .filter_map(|event| match event.id.source {
            EventSource::Execution { execution } => Some(execution),
            EventSource::Coordinator => None,
        })
        .collect();
    ids.extend(executions.into_iter().map(LogId::Execution));
    let mut stored = Vec::new();
    for id in ids {
        stored.push((id, logs.read(&id).await.expect("the log reads")));
    }
    stored
}

/// A fresh in-memory copy of the run with its graphs and no records yet.
async fn empty_copy(source: &dyn RunLogs) -> Arc<dyn RunLogs> {
    let store = MemoryRunStore::new();
    let target = store
        .open(&RunKey::new("copy"), Access::Create {
            owner: OwnerId::new("copier"),
        })
        .await
        .expect("creates");
    for record in read_coordinator_log(source).await.expect("reads") {
        if let CoordinatorEvent::GraphRegistered { digest } = record.body {
            let bytes = source
                .get_blob(digest)
                .await
                .expect("reads the graph")
                .expect("the graph is stored");
            target.put_blob(&bytes).await.expect("stores the graph");
        }
    }
    target
}

/// The events of one log, in order.
fn of_log(events: &[RunEvent], source: EventSource) -> Vec<&RunEvent> {
    events
        .iter()
        .filter(|event| event.id.source == source)
        .collect()
}

/// Two event sequences are the same, reported at the first event that
/// differs.
fn assert_same_events(got: &[&RunEvent], want: &[&RunEvent], what: &str) {
    let ids = |events: &[&RunEvent]| -> Vec<String> {
        events
            .iter()
            .map(|event| format!("{}/{}:{:?}", event.id.seq, event.id.index, event.origin))
            .collect()
    };
    let (got_all, want_all) = (got, want);
    for (index, (got, want)) in got.iter().zip(want).enumerate() {
        assert_eq!(
            got,
            want,
            "{what}: event {index} differs; got {:?}, want {:?}",
            ids(got_all),
            ids(want_all)
        );
    }
    assert_eq!(got.len(), want.len(), "{what}: the count differs");
}

/// A run's logs with one record hidden: a gap a reader finds in the log.
struct Gapped {
    inner: Arc<dyn RunLogs>,
    log:   LogId,
    seq:   u64,
}

#[async_trait::async_trait]
impl RunLogs for Gapped {
    fn locator(&self) -> String {
        self.inner.locator()
    }

    async fn append(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError> {
        self.inner.append(log, records).await
    }

    async fn read(&self, log: &LogId) -> Result<Vec<Record>, StoreError> {
        let mut records = self.inner.read(log).await?;
        if *log == self.log {
            records.retain(|record| record.seq != self.seq);
        }
        Ok(records)
    }

    async fn read_from(&self, log: &LogId, seq: u64) -> Result<Vec<Record>, StoreError> {
        let mut records = self.read(log).await?;
        records.retain(|record| record.seq >= seq);
        Ok(records)
    }

    async fn put_blob(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        self.inner.put_blob(bytes).await
    }

    async fn get_blob(&self, digest: store::Digest) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get_blob(digest).await
    }
}

/// A replay advanced over a run fed to its store in pieces derives, per
/// log, the events of the full replay in the same order, whatever the
/// piece size: one record at a time (so every external record is seen
/// before the records its apply produced, and every execution record
/// before its declaration), three at a time, and the run whole. An advance
/// over an unchanged store derives nothing.
#[tokio::test]
async fn a_replay_advanced_in_pieces_derives_the_events_of_the_full_replay() {
    let dir = RunDir::new("run-replay");
    let rt = Runtime::standard().options(RunOptions::new(dir.path()));
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = add_script(&mut b, "start", scope, "echo start");
    let left = add_script(&mut b, "left", scope, "echo left");
    let right = add_script(&mut b, "right", scope, "echo right");
    let end = add_script(&mut b, "end", scope, "echo end");
    b.fan_out(start, &[left, right]);
    b.link(left, end);
    b.link(right, end);
    b.set_join(end, JoinPolicy::All);
    let report = host::run(&rt, b.build()).await.expect("the run completes");
    assert_eq!(report.status, RunStatus::Success);

    let source = testkit::read_run_dir(dir.path()).await;
    let full = replay_run(&*source).await.expect("the run projects");
    let logs = stored_logs(&*source, &full).await;
    assert!(logs.len() >= 2, "a coordinator log and an execution log");
    let sources: BTreeSet<EventSource> = full.iter().map(|event| event.id.source).collect();

    for piece in [1, 3, usize::MAX] {
        let target = empty_copy(&*source).await;
        let mut replay = RunReplay::new();
        let mut cursors: Vec<usize> = vec![0; logs.len()];
        let mut advances = 0;
        let mut pieced: Vec<RunEvent> = Vec::new();
        loop {
            let mut fed = false;
            for (index, (log, records)) in logs.iter().enumerate() {
                let end = cursors[index].saturating_add(piece).min(records.len());
                if cursors[index] < end {
                    target
                        .append(log, &records[cursors[index]..end])
                        .await
                        .expect("the piece appends");
                    cursors[index] = end;
                    fed = true;
                }
            }
            if !fed {
                break;
            }
            pieced.extend(replay.advance(&*target).await.expect("the replay advances"));
            advances += 1;
        }
        if piece == usize::MAX {
            assert_eq!(advances, 1, "the run whole is one advance");
        } else {
            assert!(advances > 3, "the run was fed in pieces: {advances}");
        }
        for source in &sources {
            assert_same_events(
                &of_log(&pieced, *source),
                &of_log(&full, *source),
                &format!("the {source:?} log's events, fed {piece} at a time"),
            );
        }
        assert_eq!(pieced.len(), full.len(), "fed {piece} at a time");
        assert!(
            replay
                .advance(&*target)
                .await
                .expect("an advance over nothing new")
                .is_empty(),
            "nothing new derives nothing"
        );
    }
}

/// A gap in a log fails the advance the way it fails the full replay, and
/// leaves the replay where it stood: once the log is whole, the same
/// advance derives what the gap held back.
#[tokio::test]
async fn a_gap_fails_the_advance_and_leaves_the_replay_where_it_stood() {
    let dir = RunDir::new("run-replay-gap");
    let rt = Runtime::standard().options(RunOptions::new(dir.path()));
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let first = add_script(&mut b, "first", scope, "echo one");
    let second = add_script(&mut b, "second", scope, "echo two");
    b.link(first, second);
    let report = host::run(&rt, b.build()).await.expect("the run completes");
    assert_eq!(report.status, RunStatus::Success);

    let source = testkit::read_run_dir(dir.path()).await;
    let full = replay_run(&*source).await.expect("the run projects");
    let execution = full
        .iter()
        .map(|event| event.id.source)
        .find(|source| matches!(source, EventSource::Execution { .. }))
        .expect("an execution log");
    let EventSource::Execution { execution: id } = execution else {
        unreachable!("found above");
    };
    let engine = of_log(&full, execution);
    let cut = engine[engine.len() / 2].id.seq;
    let gapped = Gapped {
        inner: Arc::clone(&source),
        log:   LogId::Execution(id),
        seq:   cut,
    };

    let mut replay = RunReplay::new();
    let error = replay
        .advance(&gapped)
        .await
        .expect_err("a gap fails the advance");
    assert!(
        matches!(error, ReplayError::EngineLog(_)),
        "the gap is reported as the log's: {error}"
    );
    // The replay stood still: the next advance over the whole log is the
    // full replay.
    let events = replay
        .advance(&*source)
        .await
        .expect("the whole log advances");
    assert_eq!(events, full);

    // And a gap found after a prefix was consumed leaves that prefix held.
    let prefix = {
        let target = empty_copy(&*source).await;
        let logs = stored_logs(&*source, &full).await;
        for (log, records) in &logs {
            let end = if *log == LogId::Execution(id) {
                usize::try_from(cut).expect("a small seq")
            } else {
                records.len()
            };
            target
                .append(log, &records[..end])
                .await
                .expect("the prefix appends");
        }
        target
    };
    let mut replay = RunReplay::new();
    let held = replay.advance(&*prefix).await.expect("the prefix advances");
    let held_engine = of_log(&held, execution).len();
    assert!(held_engine > 0 && held_engine < engine.len());
    let gapped = Gapped {
        inner: Arc::clone(&source),
        log:   LogId::Execution(id),
        seq:   cut + 1,
    };
    let error = replay
        .advance(&gapped)
        .await
        .expect_err("the gap past the prefix fails the advance");
    assert!(matches!(error, ReplayError::EngineLog(_)), "{error}");
    let rest = replay
        .advance(&*source)
        .await
        .expect("the whole log advances");
    let mut pieced: Vec<&RunEvent> = held.iter().collect();
    pieced.extend(rest.iter());
    assert_eq!(
        pieced
            .iter()
            .filter(|event| event.id.source == execution)
            .copied()
            .collect::<Vec<_>>(),
        engine,
        "the prefix held plus the rest is the full engine log"
    );
}
