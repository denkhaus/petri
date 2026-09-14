//! The host-facing observer seams a store hangs off: an execution observer
//! takes part in the durability acknowledgement, and replay can start from
//! held positions.

use std::collections::BTreeMap;
use std::sync::Arc;

use driver::ObserveError;
use engine::{EngineState, EventRecord};
use execution::events::{EventId, EventSource, replay_run, replay_since};
use execution::host::{self, HostRun};
use execution::{CoordinatorRecord, ExecutionId, ExecutionObserver};
use ir::{GraphBuilder, Outcome, RunStatus, ScopeId, StepEvent};
use runtime::steps::{ProgressError, Registry, StepCtx, StepRunner};
use runtime::{RunOptions, Runtime};
use serde_json::json;
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

    let full = replay_run(dir.path()).expect("the run dir projects");
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
    let tail = replay_since(dir.path(), &held).expect("the suffix projects");
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
    let tail = replay_since(dir.path(), &held).expect("the suffix projects");
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
        replay_since(dir.path(), &BTreeMap::new()).expect("projects"),
        full
    );
}
