//! Recording times: every public event carries the wall-clock time its record
//! was appended, read at the recording boundary, and a replay of the run dir
//! recovers those same times while `observed_at` stays live-only.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use execution::events::{
    EventBody, EventId, EventProjector, RunEvent, RunEventSink, SinkError, replay_run,
};
use execution::host::{self, HostRun};
use ir::{GraphBuilder, RunStatus, ScopeId};
use runtime::driver::recorded_now;
use runtime::{RunOptions, Runtime};
use testkit::{RunDir, add_script};

#[derive(Default)]
struct Collecting(Mutex<Vec<RunEvent>>);

#[async_trait::async_trait]
impl RunEventSink for Collecting {
    async fn deliver(&self, event: RunEvent) -> Result<(), SinkError> {
        self.0.lock().expect("not poisoned").push(event);
        Ok(())
    }
}

fn time_of(events: &[RunEvent], pick: impl Fn(&EventBody) -> bool) -> u64 {
    events
        .iter()
        .find(|event| pick(&event.body))
        .and_then(|event| event.recorded_at)
        .expect("the event exists and carries its recording time")
}

#[tokio::test]
async fn replay_recovers_the_recorded_times_and_observed_at_stays_live_only() {
    let dir = RunDir::new("recorded-times");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let first = add_script(&mut b, "first", scope, "echo one");
    let second = add_script(&mut b, "second", scope, "echo two");
    b.link(first, second);
    let graph = b.build();

    let rt = Runtime::standard().options(RunOptions::new(dir.path()));
    let sink = Arc::new(Collecting::default());
    let projector = EventProjector::new(sink.clone());
    let before = recorded_now();
    let report = host::run_configured(
        &rt,
        HostRun::new(graph).observe(projector.clone()),
        |_, _| {},
    )
    .await
    .expect("the run completes");
    let after = recorded_now();
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let receipt = projector.shutdown().await;
    assert!(receipt.is_clean(), "{receipt:?}");

    let live = sink.0.lock().expect("not poisoned").clone();
    assert!(!live.is_empty());
    for event in &live {
        let recorded = event
            .recorded_at
            .expect("a live event carries the time its record was appended");
        assert!(
            (before..=after).contains(&recorded),
            "the recording time is the run's wall clock: {event:?}"
        );
        let observed = event
            .observed_at
            .expect("a live event carries the time the projector saw it");
        assert!(
            observed >= recorded,
            "a record is seen after it is recorded: {event:?}"
        );
    }
    // Within one log, recording times follow the records.
    let mut by_source: BTreeMap<_, Vec<&RunEvent>> = BTreeMap::new();
    for event in &live {
        by_source.entry(event.id.source).or_default().push(event);
    }
    for events in by_source.values_mut() {
        events.sort_by_key(|event| event.id);
        let times: Vec<u64> = events.iter().filter_map(|e| e.recorded_at).collect();
        assert!(
            times.windows(2).all(|pair| pair[0] <= pair[1]),
            "recording times never decrease along a log: {times:?}"
        );
    }
    // The boundaries a host reconstructs a timeline from are ordered.
    let run_started = time_of(&live, |body| matches!(body, EventBody::RunStarted { .. }));
    let run_finished = time_of(&live, |body| matches!(body, EventBody::RunFinished { .. }));
    assert!(run_started <= run_finished);
    let first_started = time_of(&live, |body| matches!(body, EventBody::AttemptStarted));
    let last_finished = live
        .iter()
        .filter(|event| matches!(event.body, EventBody::AttemptFinished { .. }))
        .filter_map(|event| event.recorded_at)
        .max()
        .expect("attempts finished");
    assert!(run_started <= first_started && first_started <= last_finished);
    assert!(last_finished <= run_finished);

    // Replay: the same times, read back from the logs; no observation time.
    let replayed = replay_run(dir.path()).expect("the run dir projects");
    let live_times: BTreeMap<EventId, Option<u64>> = live
        .iter()
        .map(|event| (event.id, event.recorded_at))
        .collect();
    let replayed_times: BTreeMap<EventId, Option<u64>> = replayed
        .iter()
        .map(|event| (event.id, event.recorded_at))
        .collect();
    assert_eq!(
        replayed_times, live_times,
        "every replayed event carries the time its record was appended live"
    );
    assert!(
        replayed.iter().all(|event| event.observed_at.is_none()),
        "replay observes nothing: `observed_at` stays live-only"
    );
}
