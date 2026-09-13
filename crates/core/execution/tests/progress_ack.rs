//! Acknowledged progress against the real engine-log writer: when
//! `send_acked` returns, the record is in `events.jsonl`, so a driver that
//! dies right after leaves it on disk for the resume to load — and the
//! attempt whose finish never landed runs again.

use std::sync::Arc;
use std::time::Duration;

use execution::{JsonlEngineLog, read_engine_log};
use ir::{GraphBuilder, Outcome, RunStatus, ScopeId, StepEvent};
use runtime::driver::{Driver, EventObserver, RunConfig};
use runtime::engine::{Event, EventLog, verify_replay};
use runtime::executor::MapSecrets;
use runtime::executor::sandbox::HostExecutor;
use runtime::steps::{Registry, StepCtx, StepRunner};
use serde_json::json;
use testkit::RunDir;
use tokio::sync::Notify;
use tokio::time::timeout;

const PARKING: ir::StepKindId = ir::StepKindId::new_static("parking");

/// Sends one acknowledged event, then — when told to park — announces the
/// acknowledgement and waits for a crash that never lets it finish. The
/// re-dispatched attempt, with `park` off, sends the event and finishes.
struct ParkingStep {
    acked: Arc<Notify>,
    park:  bool,
}

impl ir::StepKind for ParkingStep {
    fn id(&self) -> ir::StepKindId {
        PARKING
    }

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "parking"
    }
}

#[async_trait::async_trait]
impl StepRunner for ParkingStep {
    async fn run(&self, mut ctx: StepCtx) -> Outcome {
        ctx.logs
            .send_acked(StepEvent::Custom(json!({ "kind": "durable-test" })))
            .await
            .expect("acknowledged");
        if self.park {
            self.acked.notify_one();
            let _ = ctx.control.recv().await;
        }
        Outcome::success(json!("done"))
    }
}

fn registry(acked: &Arc<Notify>, park: bool) -> Registry {
    let mut registry = Registry::new();
    registry.register_runner(Arc::new(ParkingStep {
        acked: acked.clone(),
        park,
    }));
    registry
}

fn markers(log: &EventLog) -> usize {
    log.events()
        .filter(|event| {
            matches!(
                event,
                Event::StepProgressRecorded {
                    ev: StepEvent::Custom(value),
                    ..
                } if value["kind"] == "durable-test"
            )
        })
        .count()
}

#[tokio::test]
async fn an_acknowledged_record_is_on_disk_when_the_driver_dies() {
    let dir = RunDir::new("durable-ack-crash");
    let path = dir.path().join("events.jsonl");
    let mut b = GraphBuilder::new();
    b.add_step("work", ScopeId::new(0), PARKING);
    let graph = b.build();
    let acked = Arc::new(Notify::new());

    let writer = Arc::new(JsonlEngineLog::create(&path).expect("the log file"));
    let driver = Driver::new(
        graph.clone(),
        Arc::new(HostExecutor::new(dir.path())),
        registry(&acked, true),
        Arc::new(MapSecrets::empty()),
        RunConfig::new(dir.path()),
    )
    .observe(writer.clone() as Arc<dyn EventObserver>);
    let run = tokio::spawn(driver.run());
    timeout(Duration::from_secs(10), acked.notified())
        .await
        .expect("the acknowledgement arrives");
    // The crash: the driver, its writer and the parked step all go away.
    run.abort();
    let _ = run.await;
    drop(writer);

    let decoded = read_engine_log(&path).expect("the file decodes");
    assert!(!decoded.torn, "every record was written whole");
    assert_eq!(
        markers(&decoded.log),
        1,
        "the acknowledged record is in the file the crash left behind"
    );
    let firing = decoded
        .log
        .events()
        .find_map(|event| match event {
            Event::StepStarted { firing, .. } => Some(*firing),
            _ => None,
        })
        .expect("the attempt started");

    let high_water = decoded.log.len() as u64;
    let writer = Arc::new(JsonlEngineLog::append(&path, high_water).expect("reopened"));
    let (driver, info) = Driver::resume(
        graph.clone(),
        decoded.log,
        Arc::new(HostExecutor::new(dir.path())),
        registry(&acked, false),
        Arc::new(MapSecrets::empty()),
        RunConfig::new(dir.path()),
    )
    .expect("the on-disk log resumes");
    assert_eq!(info.redispatched, vec![firing]);
    let report = driver
        .observe(writer.clone() as Arc<dyn EventObserver>)
        .run()
        .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(
        report.observer_errors.is_empty(),
        "{:?}",
        report.observer_errors
    );

    let complete = read_engine_log(&path).expect("the file decodes");
    assert_eq!(complete.log, report.state.log, "the file is the log");
    assert_eq!(
        markers(&complete.log),
        2,
        "the crash's record and the re-dispatched attempt's: at least once"
    );
    verify_replay(graph, &report.state.log).expect("the log replays");
}
