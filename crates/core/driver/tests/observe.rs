//! The `EventObserver` seam: every appended record, in seq order, with the
//! post-apply state alongside; `finish` awaited before the report and its
//! failures surfaced without touching the run status.

mod support;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use driver::{EventObserver, ObserveError};
use engine::{EngineState, EventRecord, EventSource};
use ir::{GraphBuilder, RunStatus, ScopeId, Value};
use serde_json::json;
use support::*;

/// What the state resolved a finish record's firing to at that moment.
type ResolvedFinish = (u64, Option<(String, Value)>);

/// Records everything it sees, and what the state could resolve at that moment.
#[derive(Default)]
struct Recording {
    records:  Mutex<Vec<EventRecord>>,
    /// For each `StepFinished` record: seq, and the `(name, meta)` the state
    /// resolved the firing to — at the moment of the record, when the firing is
    /// already retired.
    finishes: Mutex<Vec<ResolvedFinish>>,
    /// Sleep this long in `on_record`, to stand in for a slow consumer.
    delay:    Option<Duration>,
}

#[async_trait::async_trait]
impl EventObserver for Recording {
    fn on_record(&self, record: &EventRecord, state: &EngineState) {
        if let Some(delay) = self.delay {
            thread::sleep(delay);
        }
        if let engine::Event::StepFinished { firing, .. } = &record.event {
            let resolved = state
                .firing_node(*firing)
                .and_then(|node| state.graph().node(node))
                .map(|n| (n.name.to_string(), n.meta.clone()));
            self.finishes
                .lock()
                .expect("not poisoned")
                .push((record.seq, resolved));
        }
        self.records
            .lock()
            .expect("not poisoned")
            .push(record.clone());
    }
}

/// Fails its `finish`, standing in for a sink whose flush failed.
struct BrokenSink;

#[async_trait::async_trait]
impl EventObserver for BrokenSink {
    fn on_record(&self, _record: &EventRecord, _state: &EngineState) {}

    async fn finish(&self) -> Result<(), ObserveError> {
        Err(ObserveError::new("broken-sink", "the flush failed"))
    }
}

fn two_step_graph() -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = add_script(&mut b, "first", scope, "echo one");
    let c = add_script(&mut b, "second", scope, "echo two");
    b.set_meta(a, json!({ "job": "build", "index": 1 }));
    b.link(a, c);
    b.build()
}

/// Two observers — one slow — both see every record, External and Core, in seq
/// order, equal to the final log. Slowness delays; it never loses.
#[tokio::test]
async fn every_observer_sees_every_record_in_seq_order() {
    let dir = RunDir::new("observe-all");
    let fast = Arc::new(Recording::default());
    let slow = Arc::new(Recording {
        delay: Some(Duration::from_millis(2)),
        ..Recording::default()
    });
    let report = host_driver(two_step_graph(), &dir)
        .observe(fast.clone() as Arc<dyn EventObserver>)
        .observe(slow.clone() as Arc<dyn EventObserver>)
        .await_run()
        .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(report.observer_errors.is_empty());

    for observer in [&fast, &slow] {
        let seen = observer.records.lock().expect("not poisoned");
        assert_eq!(
            seen.as_slice(),
            report.state.log.records(),
            "an observer saw exactly the log, in order"
        );
        assert!(seen.iter().any(|r| r.source == EventSource::External));
        assert!(
            seen.iter().any(|r| r.source == EventSource::Core),
            "Core records — routed tokens — reach observers too"
        );
        for (i, record) in seen.iter().enumerate() {
            assert_eq!(record.seq, i as u64);
        }
    }
}

/// The state argument resolves a firing to its node name and `meta` at the
/// moment of the record — including for finish records, where the firing is
/// already retired and the lookup goes through `firing_node` (live + history).
#[tokio::test]
async fn the_state_resolves_finish_records_through_history() {
    let dir = RunDir::new("observe-resolve");
    let observer = Arc::new(Recording::default());
    let report = host_driver(two_step_graph(), &dir)
        .observe(observer.clone() as Arc<dyn EventObserver>)
        .await_run()
        .await;
    assert_eq!(report.status, RunStatus::Success);

    let finishes = observer.finishes.lock().expect("not poisoned");
    assert_eq!(finishes.len(), 2, "one finish per node");
    let (_, first) = &finishes[0];
    let (name, meta) = first.as_ref().expect("the retired firing still resolves");
    assert_eq!(name, "first");
    assert_eq!(meta, &json!({ "job": "build", "index": 1 }));
    let (_, second) = &finishes[1];
    assert_eq!(
        second.as_ref().expect("resolves").0,
        "second",
        "a node with default meta resolves too"
    );
}

/// An observer whose `finish` fails: the error is in the report, the run status
/// is untouched.
#[tokio::test]
async fn a_failing_finish_is_reported_without_changing_the_status() {
    let dir = RunDir::new("observe-finish-fails");
    let mut b = GraphBuilder::new();
    add_script(&mut b, "only", ScopeId::new(0), "echo hi");
    let report = host_driver(b.build(), &dir)
        .observe(Arc::new(BrokenSink))
        .await_run()
        .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "a sink failure never fails the run"
    );
    assert_eq!(report.observer_errors.len(), 1);
    assert_eq!(report.observer_errors[0].observer, "broken-sink");
}
