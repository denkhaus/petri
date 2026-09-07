//! Readiness item 6 on the standalone host: the failure circuit breaker
//! (`loop_restart_signature_limit`), node visit totals across `loop_restart`,
//! the stall watchdog (`stall_timeout`), and the control service (pause at
//! admission, steering into a live stage). Agents are Fabro's simulated
//! stubs; commands and human gates are real.

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use petri::engine::{EngineState, Event, EventRecord, RunError};
use petri::execution::controls::{ControlError, ControlService};
use petri::execution::host::{self, HostRun};
use petri::execution::watchdog::StallWatchdog;
use petri::execution::{
    ExecutionObserver, InterviewDispatcher, InterviewReply, InterviewRequest, Interviewer,
};
use petri::executor::Retention;
use petri::fabro::{AGENT_KIND, CommandStep, HumanStep, StubStep, WAIT_KIND, WORKFLOW_KIND};
use petri::frontend::fabro::Fabro;
use petri::frontend::{CompileInputs, Lowered};
use petri::ir::{Graph, RunStatus, Value};
use petri::steps::Answer;
use petri::{RunOptions, Runtime, driver};
use serde_json::json;
use testkit::RunDir;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

fn runtime(dir: &RunDir) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    let mut registry = Runtime::standard().registry().clone();
    for kind in [&AGENT_KIND, &WAIT_KIND, &WORKFLOW_KIND] {
        registry.register_runner(Arc::new(StubStep::new((*kind).clone())));
    }
    registry.register(CommandStep);
    registry.register(HumanStep);
    Runtime::standard()
        .frontend(Fabro::new())
        .steps(registry)
        .options(options)
}

fn lower(rt: &Runtime, dir: &RunDir, text: &str) -> Lowered {
    let path = dir.path().join("wf.fabro");
    fs::write(&path, text).expect("write the workflow");
    let lowered = rt
        .check(&path, None, None, &CompileInputs::new())
        .expect("loads");
    assert!(
        lowered.graph.is_some(),
        "the workflow lowers: {:?}",
        lowered.diagnostics
    );
    lowered
}

/// Script a stub node's calls.
fn script(graph: &mut Graph, node: &str, calls: Value) {
    let node = graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == node)
        .expect("the node");
    let Value::Object(config) = &mut node.step.config else {
        panic!("an object config");
    };
    config.insert("simulate".into(), json!({ "calls": calls }));
}

fn firings_of(report: &driver::ExecutionReport, node: &str) -> usize {
    report
        .state
        .history()
        .iter()
        .filter(|r| r.name == node)
        .count()
}

// ── Circuit breaker ─────────────────────────────────────────────────────────

const RESTART_LOOP: &str = r#"digraph G {
    graph [loop_restart_signature_limit=3]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [prompt="x", max_visits=10]
    check [shape=diamond]
    start -> work -> check
    check -> exit [condition="context.done=true"]
    check -> work [loop_restart=true]
}"#;

/// Three repeats of one deterministic failure signature fail the run, and
/// the count survives the restart chain: each repeat is in its own
/// execution.
#[tokio::test]
async fn a_repeated_deterministic_failure_trips_the_breaker_across_restarts() {
    let dir = RunDir::new("breaker-trip");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, RESTART_LOOP);
    let mut graph = lowered.graph.expect("lowers");
    assert_eq!(
        graph.policy.loop_restart_signature_limit.map(|n| n.get()),
        Some(3)
    );
    script(
        &mut graph,
        "work",
        json!([{
            "outcome": "failed",
            "failure_reason": "the tests failed on line 12",
            "context_updates": { "done": "false" }
        }]),
    );
    let report = host::run_configured(&rt, HostRun::new(graph), |_, _| {})
        .await
        .expect("the run completes");
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(
        firings_of(&report, "work"),
        1,
        "the last execution ran work once"
    );
    let blocked = report.state.errors().iter().find_map(|error| match error {
        RunError::RoutingBlocked { reason, .. } => Some(reason.to_string()),
        _ => None,
    });
    let reason = blocked.expect("the route was blocked");
    assert!(
        reason.contains("deterministic failure cycle detected"),
        "{reason}"
    );
    assert!(reason.contains("repeated 3 times (limit 3)"), "{reason}");
    // Three executions ran (two restarts), one `work` each.
    let document = petri::execution::inspect::inspect_run(dir.path()).expect("inspects");
    assert_eq!(document.executions.len(), 3, "{document:?}");
}

/// Success does not clear a signature's count.
#[tokio::test]
async fn a_success_between_failures_does_not_clear_the_count() {
    let dir = RunDir::new("breaker-no-clear");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, RESTART_LOOP);
    let mut graph = lowered.graph.expect("lowers");
    let fail = json!({
        "outcome": "failed",
        "failure_reason": "the tests failed",
        "context_updates": { "done": "false" }
    });
    script(
        &mut graph,
        "work",
        json!([fail, fail, { "context_updates": { "done": "false" } }, fail]),
    );
    let report = host::run_configured(&rt, HostRun::new(graph), |_, _| {})
        .await
        .expect("the run completes");
    assert_eq!(report.status, RunStatus::Failed);
    let document = petri::execution::inspect::inspect_run(dir.path()).expect("inspects");
    assert_eq!(document.executions.len(), 4, "fail, fail, success, fail");
}

/// A `loop_restart` edge taken by the failing node itself: a deterministic
/// failure may not restart; a transient one may.
#[tokio::test]
async fn a_restart_edge_admits_only_transient_failures() {
    const SELF_RESTART: &str = r#"digraph G {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        work [prompt="x", max_visits=10]
        start -> work
        work -> work [loop_restart=true, condition="outcome=failed"]
        work -> exit
    }"#;
    let dir = RunDir::new("breaker-restart-deterministic");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, SELF_RESTART);
    let mut graph = lowered.graph.expect("lowers");
    script(
        &mut graph,
        "work",
        json!([{ "outcome": "failed", "failure_reason": "syntax error" }, {}]),
    );
    let report = host::run_configured(&rt, HostRun::new(graph), |_, _| {})
        .await
        .expect("the run completes");
    assert_eq!(report.status, RunStatus::Failed);
    let reason = report
        .state
        .errors()
        .iter()
        .find_map(|error| match error {
            RunError::RoutingBlocked { reason, .. } => Some(reason.to_string()),
            _ => None,
        })
        .expect("blocked");
    assert!(
        reason.contains("loop_restart blocked: failure_class=deterministic"),
        "{reason}"
    );

    let dir = RunDir::new("breaker-restart-transient");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, SELF_RESTART);
    let mut graph = lowered.graph.expect("lowers");
    script(
        &mut graph,
        "work",
        json!([{ "outcome": "failed", "failure_reason": "connection refused by the registry" }, {}]),
    );
    let report = host::run_configured(&rt, HostRun::new(graph), |_, _| {})
        .await
        .expect("the run completes");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let document = petri::execution::inspect::inspect_run(dir.path()).expect("inspects");
    assert_eq!(document.executions.len(), 2, "one restart, then success");
}

/// Node visit totals survive a `loop_restart` while the context is
/// replaced: `max_visits=2` is reached on the third execution's visit, not
/// reset by the restart.
#[tokio::test]
async fn node_visit_totals_survive_a_restart_while_context_resets() {
    const RESTART: &str = r#"digraph G {
        graph [loop_restart_signature_limit=10]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        work [prompt="x", max_visits=2]
        check [shape=diamond]
        start -> work -> check
        check -> exit [condition="context.done=true"]
        check -> work [loop_restart=true]
    }"#;
    let dir = RunDir::new("breaker-visits");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, RESTART);
    let mut graph = lowered.graph.expect("lowers");
    script(
        &mut graph,
        "work",
        json!([{ "context_updates": { "done": "false", "seen": "yes" } }]),
    );
    let report = host::run_configured(&rt, HostRun::new(graph), |_, _| {})
        .await
        .expect("the run completes");
    assert_eq!(report.status, RunStatus::Failed);
    assert!(
        report
            .state
            .errors()
            .iter()
            .any(|error| matches!(error, RunError::BudgetExceeded { max_firings: 2, .. })),
        "{:?}",
        report.state.errors()
    );
    let document = petri::execution::inspect::inspect_run(dir.path()).expect("inspects");
    assert_eq!(document.executions.len(), 3);
    // The final execution started with an empty context: the restart
    // replaced it, and `seen` was written again by nothing.
    assert!(
        report.state.run_context().get("seen").is_none(),
        "{:?}",
        report.state.run_context().kv
    );
}

/// Both signature maps are restored on resume: a run crashed after the third
/// failure was recorded but before its route was resolved still blocks the
/// route when resumed.
#[tokio::test]
async fn the_breaker_state_is_restored_on_resume() {
    let dir = RunDir::new("breaker-resume");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, RESTART_LOOP);
    let mut graph = lowered.graph.expect("lowers");
    script(
        &mut graph,
        "work",
        json!([{
            "outcome": "failed",
            "failure_reason": "the tests failed",
            "context_updates": { "done": "false" }
        }]),
    );
    let report = host::run_configured(&rt, HostRun::new(graph), |_, _| {})
        .await
        .expect("the run completes");
    assert_eq!(report.status, RunStatus::Failed);

    // Crash the last execution right after `work`'s final StepFinished:
    // the block decision is not in the log any more.
    let executions = dir
        .path()
        .join("invocations")
        .join(format!("{:016x}", 0))
        .join("executions");
    let mut dirs: Vec<_> = fs::read_dir(&executions)
        .expect("executions")
        .map(|e| e.expect("entry").path())
        .collect();
    dirs.sort();
    let events = dirs.last().expect("an execution").join("events.jsonl");
    let text = fs::read_to_string(&events).expect("events");
    let lines: Vec<&str> = text.lines().collect();
    let cut = lines
        .iter()
        .position(|line| line.contains("\"StepFinished\"") && line.contains("Failure"))
        .expect("the failure record")
        + 1;
    fs::write(&events, format!("{}\n", lines[..cut].join("\n"))).expect("truncate");
    // The coordinator log must also forget the exit and the run's finish.
    let coordinator = dir.path().join("coordinator.jsonl");
    let text = fs::read_to_string(&coordinator).expect("coordinator log");
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            !(line.contains("\"RunFinished\"")
                || line.contains("\"InvocationFinished\"")
                || (line.contains("\"ExecutionFinished\"") && line.contains("Terminal")))
        })
        .collect();
    fs::write(&coordinator, format!("{}\n", kept.join("\n"))).expect("rewrite");

    let resumed = host::resume(&rt).await.expect("resumes");
    assert_eq!(resumed.status, RunStatus::Failed);
    let reason = resumed
        .state
        .errors()
        .iter()
        .find_map(|error| match error {
            RunError::RoutingBlocked { reason, .. } => Some(reason.to_string()),
            _ => None,
        })
        .expect("the restored count blocks the route");
    assert!(reason.contains("repeated 3 times"), "{reason}");
}

// ── Stall watchdog ──────────────────────────────────────────────────────────

/// A run with no execution activity for the stall budget is cancelled.
#[tokio::test]
async fn an_idle_run_is_cancelled_by_the_watchdog() {
    const IDLE: &str = r#"digraph G {
        graph [stall_timeout="400ms"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        long [shape=parallelogram, script="sleep 20"]
        start -> long -> exit
    }"#;
    let dir = RunDir::new("watchdog-idle");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, IDLE);
    let graph = lowered.graph.expect("lowers");
    assert_eq!(graph.policy.stall_timeout, Some(Duration::from_millis(400)));
    let watchdog = StallWatchdog::new(graph.policy.stall_timeout.expect("a budget"));
    let host_run = HostRun::new(graph).observe(Arc::new(watchdog.clone()));
    let mut task = None;
    let started = Instant::now();
    let report = host::run_configured(&rt, host_run, |handle, _| {
        task = Some(watchdog.start(handle));
    })
    .await
    .expect("the run completes");
    task.expect("started").stop().await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(report.status, RunStatus::Cancelled);
    let stall = watchdog.tripped().expect("the watchdog fired");
    assert_eq!(stall.stall_timeout_ms, 400);
    assert!(stall.idle_ms >= 400);
}

/// A pending question parks the watchdog: a gate that waits far longer than
/// the stall budget is not a stall, and the run continues once answered.
#[tokio::test]
async fn a_pending_question_parks_the_watchdog() {
    const GATED: &str = r#"digraph G {
        graph [stall_timeout="1s"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        gate [shape=hexagon, label="Go?"]
        go [shape=parallelogram, script="echo go"]
        start -> gate
        gate -> go [label="[Y] Yes"]
        go -> exit
    }"#;
    struct Slow;
    #[async_trait::async_trait]
    impl Interviewer for Slow {
        async fn reply(&self, _: InterviewRequest, cancel: CancellationToken) -> InterviewReply {
            tokio::select! {
                () = sleep(Duration::from_millis(3000)) => InterviewReply::Answered(Answer::choice("Y")),
                () = cancel.cancelled() => InterviewReply::Cancelled,
            }
        }
    }
    let dir = RunDir::new("watchdog-parked");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, GATED);
    let graph = lowered.graph.expect("lowers");
    let watchdog = StallWatchdog::new(graph.policy.stall_timeout.expect("a budget"));
    let dispatcher = InterviewDispatcher::new(Arc::new(Slow));
    let host_run = HostRun::new(graph)
        .observe(Arc::new(watchdog.clone()))
        .observe(Arc::new(dispatcher.clone()));
    let mut task = None;
    let report = host::run_configured(&rt, host_run, |handle, secrets| {
        dispatcher.wire(handle.clone(), secrets);
        task = Some(watchdog.start(handle));
    })
    .await
    .expect("the run completes");
    task.expect("started").stop().await;
    let receipt = dispatcher.shutdown().await;
    assert!(
        watchdog.tripped().is_none(),
        "a blocked run never stalls: {:?}",
        watchdog.tripped()
    );
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
}

// ── Controls ────────────────────────────────────────────────────────────────

/// Which nodes started, in order.
#[derive(Default)]
struct Starts(Mutex<Vec<String>>);

impl ExecutionObserver for Starts {
    fn on_engine_record(
        &self,
        _: petri::execution::ExecutionId,
        record: &EventRecord,
        state: &EngineState,
    ) {
        if let Event::StepStarted { firing, .. } = &record.event
            && let Some(name) = state
                .firing_node(*firing)
                .and_then(|id| state.graph().node(id))
                .map(|node| node.name.to_string())
        {
            self.0.lock().expect("not poisoned").push(name);
        }
    }

    fn on_lifecycle(&self, _: &petri::execution::CoordinatorRecord) {}
}

/// A paused run admits no new attempt; unpausing releases them, and the
/// held firing kept its identity (one visit, one attempt).
#[tokio::test]
async fn pause_holds_admission_and_unpause_releases_it() {
    const TWO: &str = r#"digraph G {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [shape=parallelogram, script="echo a"]
        b [shape=parallelogram, script="echo b"]
        start -> a -> b -> exit
    }"#;
    let dir = RunDir::new("controls-pause");
    let controls = ControlService::new();
    let rt = runtime(&dir).hooks(controls.hooks(None));
    let lowered = lower(&rt, &dir, TWO);
    let starts = Arc::new(Starts::default());
    let host_run = HostRun::new(lowered.graph.expect("lowers"))
        .observe(Arc::new(controls.clone()))
        .observe(starts.clone());
    controls.pause();
    let paused = controls.clone();
    let seen = starts.clone();
    let report = host::run_configured(&rt, host_run, |handle, _| {
        paused.wire(handle);
        tokio::spawn(async move {
            sleep(Duration::from_millis(700)).await;
            assert!(
                seen.0.lock().expect("not poisoned").is_empty(),
                "nothing started while paused"
            );
            assert!(paused.is_paused());
            paused.unpause();
        });
    })
    .await
    .expect("the run completes");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let started = starts.0.lock().expect("not poisoned").clone();
    assert_eq!(started, ["start", "a", "b", "exit"]);
    for record in report.state.history() {
        assert_eq!(record.attempt.raw(), 1, "{}", record.name);
    }
}

/// Cancellation stays responsive during a pause: the held firing settles as
/// cancelled and no attempt ever starts.
#[tokio::test]
async fn a_paused_run_can_still_be_cancelled() {
    const ONE: &str = r#"digraph G {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [shape=parallelogram, script="echo a > ran.txt"]
        start -> a -> exit
    }"#;
    let dir = RunDir::new("controls-pause-cancel");
    let controls = ControlService::new();
    let rt = runtime(&dir).hooks(controls.hooks(None));
    let lowered = lower(&rt, &dir, ONE);
    let host_run = HostRun::new(lowered.graph.expect("lowers")).observe(Arc::new(controls.clone()));
    controls.pause();
    let paused = controls.clone();
    let report = host::run_configured(&rt, host_run, |handle, _| {
        paused.wire(handle);
        tokio::spawn(async move {
            sleep(Duration::from_millis(300)).await;
            paused.cancel().expect("the run is live");
        });
    })
    .await
    .expect("the run completes");
    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(
        !report.state.history().iter().any(|r| r.name == "a"
            && r.attempt.raw() > 0
            && r.outcome.status == petri::ir::Status::Success),
        "`a` never ran"
    );
    assert!(!dir.workspace().join("ran.txt").exists());
}

/// A steer reaches the named stage's live firing and never answers its
/// question; the answer still comes from the interviewer.
#[tokio::test]
async fn a_steer_reaches_the_stage_and_does_not_answer_its_question() {
    const GATE: &str = r#"digraph G {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        gate [shape=hexagon, label="Go?"]
        go [shape=parallelogram, script="echo go"]
        start -> gate
        gate -> go [label="[Y] Yes"]
        go -> exit
    }"#;
    struct AfterSteer {
        controls: ControlService,
        steered:  Mutex<Option<Result<(), ControlError>>>,
    }
    #[async_trait::async_trait]
    impl Interviewer for AfterSteer {
        async fn reply(&self, request: InterviewRequest, _: CancellationToken) -> InterviewReply {
            // Steer the stage while its question is open, then answer.
            let result = self.controls.steer(&request.node, "hurry up").await;
            *self.steered.lock().expect("not poisoned") = Some(result);
            sleep(Duration::from_millis(200)).await;
            InterviewReply::Answered(Answer::choice("Y"))
        }
    }
    let dir = RunDir::new("controls-steer");
    let controls = ControlService::new();
    let rt = runtime(&dir).hooks(controls.hooks(None));
    let lowered = lower(&rt, &dir, GATE);
    let interviewer = Arc::new(AfterSteer {
        controls: controls.clone(),
        steered:  Mutex::new(None),
    });
    let dispatcher = InterviewDispatcher::new(interviewer.clone());
    let host_run = HostRun::new(lowered.graph.expect("lowers"))
        .observe(Arc::new(controls.clone()))
        .observe(Arc::new(dispatcher.clone()));
    let report = host::run_configured(&rt, host_run, |handle, secrets| {
        dispatcher.wire(handle.clone(), secrets);
        controls.wire(handle);
    })
    .await
    .expect("the run completes");
    let receipt = dispatcher.shutdown().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        *interviewer.steered.lock().expect("not poisoned"),
        Some(Ok(()))
    );
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    assert_eq!(receipt.questions.len(), 1);
    let deliveries = report
        .state
        .log
        .events()
        .filter(|e| matches!(e, Event::ControlRequested { .. }))
        .count();
    assert_eq!(deliveries, 2, "the steer and the answer");
    assert_eq!(
        controls.steer("gate", "too late").await,
        Err(ControlError::NoSuchStage("gate".into()))
    );
    let _: BTreeMap<_, _> = controls.stages();
}
