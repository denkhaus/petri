//! `RunHandle::deliver`: the host delivers a value into a live firing, through
//! the engine, so question and answer are both in the log. Delivery is reliable
//! — the per-firing forwarder awaits channel capacity — and the disposition
//! reports how it landed: `Delivered` once the value is in the firing's control
//! channel, `NotLive` when the firing was gone or ended first.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use driver::{CONTROL_CHANNEL_CAPACITY, DeliverDisposition, RunConfig, RunHandle};
use executor::{MapSecrets, SecretProvider};
use ir::{Control, FiringId, Graph, GraphBuilder, Outcome, RunStatus, ScopeId, Value};
use serde_json::json;
use steps::{PROCESS_KIND, Registry};
use support::*;

// ── Test step kinds ───────────────────────────────────────────────────────

/// Holds off reading its control channel until `start_file` exists, then reads
/// `count` delivered values and returns them in arrival order.
struct CollectStep;

const COLLECT_KIND: ir::StepKindId = ir::StepKindId::new_static("collect");

impl ir::StepKind for CollectStep {
    fn id(&self) -> ir::StepKindId {
        COLLECT_KIND
    }
    fn name(&self) -> &str {
        "collect"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for CollectStep {
    async fn run(&self, mut ctx: steps::StepCtx) -> Outcome {
        let count = ctx.config["count"].as_u64().unwrap_or(0) as usize;
        if let Some(path) = ctx.config["start_file"].as_str() {
            assert!(
                wait_for_file(std::path::Path::new(path), Duration::from_secs(10)).await,
                "the start file never appeared"
            );
        }
        let mut got = Vec::with_capacity(count);
        while got.len() < count {
            match ctx.control.recv().await {
                Some(Control::Deliver(value)) => got.push(value),
                Some(_) => return Outcome::cancelled(),
                None => return Outcome::failure("the control channel closed early"),
            }
        }
        Outcome::success(Value::Array(got))
    }
}

/// Never reads its control channel; returns once `finish_file` exists. What a
/// step busy with real work looks like to the delivery path.
struct BusyStep;

const BUSY_KIND: ir::StepKindId = ir::StepKindId::new_static("busy");

impl ir::StepKind for BusyStep {
    fn id(&self) -> ir::StepKindId {
        BUSY_KIND
    }
    fn name(&self) -> &str {
        "busy"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for BusyStep {
    async fn run(&self, ctx: steps::StepCtx) -> Outcome {
        let path = ctx.config["finish_file"].as_str().expect("finish_file");
        assert!(
            wait_for_file(std::path::Path::new(path), Duration::from_secs(10)).await,
            "the finish file never appeared"
        );
        Outcome::success(json!("done"))
    }
}

// ── Scaffolding ───────────────────────────────────────────────────────────

fn single_node(kind: ir::StepKindId, config: Value) -> Graph {
    let mut b = GraphBuilder::new();
    b.add_node("gate", ScopeId::new(0), ir::StepRef::new(kind, config));
    b.build()
}

/// The first firing of a run is always `FiringId(1)`.
const FIRST: FiringId = FiringId::new(1);

fn gate_registry(received: &Arc<Mutex<Vec<Value>>>) -> Registry {
    let mut registry = runners();
    registry.register_runner(Arc::new(GateStep {
        received: Arc::clone(received),
    }));
    registry
}

async fn deliver(handle: &RunHandle, firing: FiringId, value: Value) -> DeliverDisposition {
    handle.deliver(firing, Control::Deliver(value)).await
}

// ── The battery ───────────────────────────────────────────────────────────

/// The minimal human-gate shape, end to end: a step waits on `ctx.control` for
/// `Deliver(answer)` and returns it as its output. Question and answer are both
/// in the log, replay is byte-identical, and a delivery after the run is
/// `NotLive`.
#[tokio::test]
async fn a_gate_step_receives_an_answer_end_to_end() {
    let dir = RunDir::new("deliver-gate");
    let received = Arc::new(Mutex::new(Vec::new()));
    let graph = single_node(GATE_KIND, Value::Null);
    let driver = host_driver_full(
        graph.clone(),
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()),
        gate_registry(&received),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    let answer = json!({"answer": "approved", "by": "a human"});
    assert_eq!(
        deliver(&handle, FIRST, answer.clone()).await,
        DeliverDisposition::Delivered
    );

    let report = run.await.expect("the run task");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "gate"), answer);
    assert_eq!(*received.lock().expect("not poisoned"), vec![answer]);

    // Question and answer are in the log, and replay reproduces the run.
    assert!(
        report
            .state
            .log
            .events()
            .any(|e| matches!(e, engine::Event::ControlRequested { .. })),
        "the request is in the log"
    );
    engine::verify_replay(graph, &report.state.log).expect("byte-identical replay");

    // The run is over: a late answer reports `NotLive`, and nothing breaks.
    assert_eq!(
        deliver(&handle, FIRST, json!("late")).await,
        DeliverDisposition::NotLive
    );
}

/// Reliable delivery under backpressure: capacity-plus-one deliveries all
/// arrive, in send order, while the driver loop stays live — a delivery to an
/// unknown firing still gets its `NotLive` answer while the overflow send is
/// parked on the forwarder.
#[tokio::test]
async fn ordering_and_liveness_hold_when_the_control_channel_is_full() {
    let dir = RunDir::new("deliver-order");
    let start_file = dir.path().join("start");
    let count = CONTROL_CHANNEL_CAPACITY + 1;
    let graph = single_node(
        COLLECT_KIND,
        json!({ "count": count, "start_file": start_file.to_string_lossy() }),
    );
    let mut registry = runners();
    registry.register_runner(Arc::new(CollectStep));
    let driver = host_driver_full(
        graph,
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()),
        registry,
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    // The step reads nothing yet, so these fill the channel to capacity.
    for n in 0..CONTROL_CHANNEL_CAPACITY {
        assert_eq!(
            deliver(&handle, FIRST, json!(n)).await,
            DeliverDisposition::Delivered,
            "delivery {n} fits in the channel"
        );
    }
    // Capacity-plus-one: this send parks on the forwarder until the step reads.
    let overflow = {
        let handle = handle.clone();
        tokio::spawn(async move { deliver(&handle, FIRST, json!(CONTROL_CHANNEL_CAPACITY)).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!overflow.is_finished(), "the overflow delivery is waiting");

    // The driver loop is live meanwhile: an unrelated request gets its answer.
    assert_eq!(
        deliver(&handle, FiringId::new(4242), json!("nobody")).await,
        DeliverDisposition::NotLive
    );

    // Let the step drain; every value arrives, in send order.
    std::fs::write(&start_file, b"go").expect("start file");
    assert_eq!(
        overflow.await.expect("the overflow task"),
        DeliverDisposition::Delivered
    );
    let report = run.await.expect("the run task");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let want: Vec<Value> = (0..count).map(|n| json!(n)).collect();
    assert_eq!(output_of(&report, "gate"), Value::Array(want));
}

/// The race: the firing finishes while a delivery is waiting out backpressure.
/// The parked send resolves `NotLive` and the run is unaffected.
#[tokio::test]
async fn a_firing_that_finishes_first_turns_a_parked_delivery_not_live() {
    let dir = RunDir::new("deliver-race");
    let finish_file = dir.path().join("finish");
    let graph = single_node(
        BUSY_KIND,
        json!({ "finish_file": finish_file.to_string_lossy() }),
    );
    let mut registry = runners();
    registry.register_runner(Arc::new(BusyStep));
    let driver = host_driver_full(
        graph,
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()),
        registry,
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    // The step never reads: fill the channel, then park one on the forwarder.
    for n in 0..CONTROL_CHANNEL_CAPACITY {
        assert_eq!(
            deliver(&handle, FIRST, json!(n)).await,
            DeliverDisposition::Delivered
        );
    }
    let parked = {
        let handle = handle.clone();
        tokio::spawn(async move { deliver(&handle, FIRST, json!("parked")).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!parked.is_finished());

    // The firing finishes; the parked delivery's receiver is gone.
    std::fs::write(&finish_file, b"done").expect("finish file");
    assert_eq!(
        parked.await.expect("the parked task"),
        DeliverDisposition::NotLive
    );
    let report = run.await.expect("the run task");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "gate"), json!("done"));
}

/// `Deliver` never starts the cancellation ladder: no hard deadline is armed,
/// so a step that keeps working past `grace + slack` after its answer still
/// finishes on its own terms.
#[tokio::test]
async fn a_delivery_arms_no_hard_deadline() {
    let dir = RunDir::new("deliver-no-deadline");
    let received = Arc::new(Mutex::new(Vec::new()));
    let graph = single_node(GATE_KIND, json!({ "linger_ms": 600 }));
    let mut config = RunConfig::new(dir.path());
    config.grace = Duration::from_millis(200);
    config.hard_deadline_slack = Duration::from_millis(100);
    let driver = host_driver_full(
        graph,
        &dir,
        MapSecrets::empty(),
        config,
        gate_registry(&received),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert_eq!(
        deliver(&handle, FIRST, json!("carry on")).await,
        DeliverDisposition::Delivered
    );
    let report = run.await.expect("the run task");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        output_of(&report, "gate"),
        json!("carry on"),
        "the step ran past grace + slack and returned normally"
    );
}

/// A delivery into a running process step is not a stop. The step has nothing
/// to hand the value to, so it drops it and the script runs to its natural end:
/// `Success`, with no `cancel_escalation` — not `Cancelled` after a SIGTERM.
#[tokio::test]
async fn a_delivery_does_not_terminate_a_process_step() {
    let dir = RunDir::new("deliver-process");
    let workspace = dir.workspace();
    let graph = single_node(
        PROCESS_KIND,
        script(
            r#"
echo ready > ready
while [ ! -f go ]; do sleep 0.05; done
echo "saw=go" > "$CI_OUTPUT"
"#,
        ),
    );
    let driver = host_driver_with(
        graph.clone(),
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(
        wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await,
        "the script never started"
    );
    assert_eq!(
        deliver(&handle, FIRST, json!("not for you")).await,
        DeliverDisposition::Delivered
    );
    // The script is still waiting on us: it was not signalled by the delivery.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!run.is_finished(), "the step ended before it was released");
    std::fs::write(workspace.join("go"), b"go").expect("go file");

    let report = run.await.expect("the run task");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "gate");
    assert_eq!(output["saw"], json!("go"), "the script ran to its end");
    assert_eq!(output["exit_status"], json!(0));
    assert!(
        output.get("cancel_escalation").is_none(),
        "the delivery started the ladder: {output}"
    );
    assert_replay_identical(&graph, &report);
}

const SECRET: &str = "hunter2-but-long-enough-to-mask";

/// The decisive secret test, asserting both sides: a secret registered after
/// run start crosses as a reference, resolves at dispatch into the step — which
/// receives the original value — while the serialized log and state hold no
/// secret bytes.
#[tokio::test]
async fn a_sensitive_answer_crosses_as_a_reference_and_never_enters_the_log() {
    let dir = RunDir::new("deliver-secret");
    let received = Arc::new(Mutex::new(Vec::new()));
    let graph = single_node(GATE_KIND, Value::Null);
    let secrets = Arc::new(MapSecrets::empty());
    let driver = host_driver_shared(
        graph,
        &dir,
        Arc::clone(&secrets),
        RunConfig::new(dir.path()),
        gate_registry(&received),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    // The answer path: the host registers the dynamic value, then injects the
    // reference. Registration after run start is the whole point.
    secrets.register("answer:1", SECRET).expect("registered");
    assert_eq!(
        deliver(&handle, FIRST, json!({ "$secret": "answer:1" })).await,
        DeliverDisposition::Delivered
    );

    let report = run.await.expect("the run task");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    // The step received the original resolved value...
    assert_eq!(
        *received.lock().expect("not poisoned"),
        vec![json!(SECRET)],
        "the step saw the real value, not a mask"
    );

    // ...and no secret bytes were persisted anywhere: the log keeps the reference,
    // and the masker catches the step's output.
    let log_bytes = serde_json::to_string(&report.state.log).expect("encode");
    assert!(
        !log_bytes.contains(SECRET),
        "the secret leaked into the log"
    );
    let state_bytes = serde_json::to_string(&report.state).expect("encode");
    assert!(
        !state_bytes.contains(SECRET),
        "the secret leaked into the state"
    );
    assert!(
        log_bytes.contains("answer:1"),
        "the reference is what was logged"
    );
    assert_eq!(output_of(&report, "gate"), json!("***"), "output is masked");

    // Duplicate names are rejected, so an answer id can never shadow a secret.
    assert!(secrets.register("answer:1", "something-else").is_err());
}

/// An unresolvable reference at dispatch — the post-resume shape, where dynamic
/// values are gone and the host must re-provide them — fails the step with the
/// existing `secret_unavailable` class.
#[tokio::test]
async fn an_unresolvable_reference_fails_the_step() {
    let dir = RunDir::new("deliver-missing-secret");
    let received = Arc::new(Mutex::new(Vec::new()));
    let graph = single_node(GATE_KIND, Value::Null);
    let driver = host_driver_full(
        graph,
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()),
        gate_registry(&received),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert_eq!(
        deliver(&handle, FIRST, json!({ "$secret": "answer:missing" })).await,
        DeliverDisposition::NotLive
    );

    let report = run.await.expect("the run task");
    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "gate")
        .expect("the gate recorded");
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some(steps::SECRET_UNAVAILABLE_CLASS)
    );
    assert!(
        received.lock().expect("not poisoned").is_empty(),
        "nothing reached the step"
    );
}
