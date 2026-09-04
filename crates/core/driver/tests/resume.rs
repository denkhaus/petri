//! Resume from the log: rebuild by replay, re-issue what is still owed, and
//! keep every determinism claim intact. Crash states are built by running to
//! completion and truncating the log at chosen seqs.

mod support;

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use driver::{
    CANCELLED_BEFORE_RESUME, Driver, ExecutionReport, KILLED_BEFORE_RESUME, ResumeError,
    ResumeInfo, RunConfig, RunHandle,
};
use engine::{CANCEL_ESCALATION_KEY, Command, Event, EventLog, EventRecord, EventSource};
use executor::SecretProvider as _;
use ir::{
    Arm, BinOp, Control, FiringId, Graph, GraphBuilder, Outcome, RunStatus, ScopeId, StepRef, Value,
};
use serde_json::json;
use steps::Registry;
use support::*;
use tokio::time;

// ── Helpers over the log ──────────────────────────────────────────────────

fn started_count(report: &ExecutionReport, firing: FiringId) -> usize {
    report
        .state
        .log
        .records()
        .iter()
        .filter(|r| matches!(&r.event, Event::StepStarted { firing: f, .. } if *f == firing))
        .count()
}

fn escalation_of(report: &ExecutionReport, name: &str) -> Option<String> {
    report
        .state
        .history()
        .iter()
        .find(|r| r.name == name)
        .and_then(|r| r.outcome.output.get(CANCEL_ESCALATION_KEY))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Resume over the host executor, mirroring `host_driver_full`.
fn resume_driver(
    graph: Graph,
    log: EventLog,
    dir: &RunDir,
    registry: Registry,
) -> (Driver, ResumeInfo) {
    resume_driver_shared(
        graph,
        log,
        dir,
        Arc::new(executor::MapSecrets::empty()),
        registry,
    )
}

fn resume_driver_shared(
    graph: Graph,
    log: EventLog,
    dir: &RunDir,
    secrets: Arc<executor::MapSecrets>,
    registry: Registry,
) -> (Driver, ResumeInfo) {
    let executor: Arc<dyn executor::Executor> =
        Arc::new(executor_sandbox::HostExecutor::new(dir.path()));
    Driver::resume(
        graph,
        log,
        executor,
        registry,
        secrets,
        RunConfig::new(dir.path()),
    )
    .expect("the log resumes")
}

// ── Test step kinds ───────────────────────────────────────────────────────

/// Counts its executions; succeeds.
struct CountingStep {
    runs: Arc<AtomicUsize>,
}

const COUNTING: ir::StepKindId = ir::StepKindId::new_static("counting");

impl ir::StepKind for CountingStep {
    fn id(&self) -> ir::StepKindId {
        COUNTING
    }
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "counting"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for CountingStep {
    async fn run(&self, _ctx: steps::StepCtx) -> Outcome {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Outcome::success(json!("ok"))
    }
}

/// Fails its first attempt, succeeds after — deterministic across a resume,
/// because it keys off the attempt rather than shared state.
struct FlakyStep {
    runs: Arc<AtomicUsize>,
}

const FLAKY: ir::StepKindId = ir::StepKindId::new_static("flaky");

impl ir::StepKind for FlakyStep {
    fn id(&self) -> ir::StepKindId {
        FLAKY
    }
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "flaky"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for FlakyStep {
    async fn run(&self, ctx: steps::StepCtx) -> Outcome {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if ctx.attempt.raw() == 1 {
            Outcome::failure("first attempt fails")
        } else {
            Outcome::success(json!("second attempt"))
        }
    }
}

/// Writes a marker file so the test can stop the run mid-step, then waits on
/// its control channel: `Cancel` ends it politely, `Kill` only if `hard` is
/// off. With `hard` set it ignores `Cancel` and returns only on `Kill`.
struct WaitingStep {
    runs: Arc<AtomicUsize>,
    hard: bool,
}

const WAITING: ir::StepKindId = ir::StepKindId::new_static("waiting");

impl ir::StepKind for WaitingStep {
    fn id(&self) -> ir::StepKindId {
        WAITING
    }
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "waiting"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for WaitingStep {
    async fn run(&self, mut ctx: steps::StepCtx) -> Outcome {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if let Some(path) = ctx.config.get("marker").and_then(|v| v.as_str()) {
            fs::write(path, b"here").expect("marker");
        }
        loop {
            match ctx.control.recv().await {
                Some(Control::Cancel) if !self.hard => return Outcome::cancelled(),
                Some(Control::Kill) => return Outcome::cancelled(),
                Some(_) => {}
                None => return Outcome::failure("control channel closed"),
            }
        }
    }
}

fn registry_with(runner: Arc<dyn steps::StepRunner>) -> Registry {
    let mut registry = runners();
    registry.register_runner(runner);
    registry
}

// ── The battery ───────────────────────────────────────────────────────────

fn counting_chain() -> (Graph, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let first = b.add_step("first", scope, COUNTING);
    let second = b.add_node(
        "second",
        scope,
        StepRef::new(ir::StepKindId::new_static("counting2"), Value::Null),
    );
    b.link(first, second);
    (
        b.build(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
}

struct Counting2(Arc<AtomicUsize>);

impl ir::StepKind for Counting2 {
    fn id(&self) -> ir::StepKindId {
        ir::StepKindId::new_static("counting2")
    }
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "counting2"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for Counting2 {
    async fn run(&self, _ctx: steps::StepCtx) -> Outcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        Outcome::success(json!("ok"))
    }
}

fn chain_registry(first: &Arc<AtomicUsize>, second: &Arc<AtomicUsize>) -> Registry {
    let mut registry = runners();
    registry.register_runner(Arc::new(CountingStep {
        runs: first.clone(),
    }));
    registry.register_runner(Arc::new(Counting2(second.clone())));
    registry
}

async fn run_chain(
    graph: &Graph,
    dir: &RunDir,
    first: &Arc<AtomicUsize>,
    second: &Arc<AtomicUsize>,
) -> ExecutionReport {
    let report = host_driver_full(
        graph.clone(),
        dir,
        executor::MapSecrets::empty(),
        RunConfig::new(dir.path()),
        chain_registry(first, second),
    )
    .await_run()
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    report
}

/// Happy path: truncate mid-run — the second step live and started — resume,
/// the step runs again, exactly one finish record and no duplicate
/// `StepStarted`, `ResumeInfo` names exactly the re-dispatched firing, and the
/// final log verifies.
#[tokio::test]
async fn a_started_firing_is_redispatched_without_a_second_ack() {
    let dir = RunDir::new("resume-started");
    let (graph, first, second) = counting_chain();
    let report = run_chain(&graph, &dir, &first, &second).await;

    let target = firing_of(&report, "second");
    let cut = finish_seq(&report.state.log, target);
    let prefix = report.state.log.prefix(cut);

    let (driver, info) =
        resume_driver(graph.clone(), prefix, &dir, chain_registry(&first, &second));
    assert_eq!(info.redispatched, vec![target]);
    assert_eq!(info.loaded, cut);
    let resumed = driver.await_run().await;
    assert_eq!(
        resumed.status,
        RunStatus::Success,
        "{:?}",
        resumed.state.errors()
    );

    assert_eq!(
        first.load(Ordering::SeqCst),
        1,
        "the finished step never re-runs"
    );
    assert_eq!(second.load(Ordering::SeqCst), 2, "the live step ran again");
    assert_eq!(
        started_count(&resumed, target),
        1,
        "no duplicate StepStarted"
    );
    assert_one_terminal_per_firing(&resumed);
    assert_replay_identical(&graph, &resumed);
}

/// An unstarted firing — the crash beat the `StepStarted` ack — gets its ack on
/// resume: exactly one in the final log.
#[tokio::test]
async fn an_unstarted_firing_gets_its_ack_on_resume() {
    let dir = RunDir::new("resume-unstarted");
    let (graph, first, second) = counting_chain();
    let report = run_chain(&graph, &dir, &first, &second).await;

    let target = firing_of(&report, "second");
    let ack = seq_of(
        &report.state.log,
        |r| matches!(&r.event, Event::StepStarted { firing, .. } if *firing == target),
    );
    let prefix = report.state.log.prefix(ack);

    let (driver, info) =
        resume_driver(graph.clone(), prefix, &dir, chain_registry(&first, &second));
    assert_eq!(info.redispatched, vec![target]);
    let resumed = driver.await_run().await;
    assert_eq!(resumed.status, RunStatus::Success);
    assert_eq!(
        started_count(&resumed, target),
        1,
        "exactly one ack, from the resume"
    );
    assert_eq!(second.load(Ordering::SeqCst), 2);
    assert_replay_identical(&graph, &resumed);
}

/// Observer that remembers every `(seq, source)` it is handed, in order.
#[derive(Default)]
struct SeqObserver {
    seen: Mutex<Vec<(u64, EventSource)>>,
}

#[async_trait::async_trait]
impl driver::EventObserver for SeqObserver {
    fn on_record(&self, record: &EventRecord, _state: &engine::EngineState) {
        self.seen
            .lock()
            .expect("not poisoned")
            .push((record.seq, record.source));
    }
}

/// A crash that lands between an External append and the flush of the Core
/// records it derived: observers attached at resume see the regenerated suffix
/// — Core records included — before any new record, each seq exactly once.
#[tokio::test]
async fn observers_see_the_regenerated_suffix_first() {
    let dir = RunDir::new("resume-suffix");
    let (graph, first, second) = counting_chain();
    let report = run_chain(&graph, &dir, &first, &second).await;

    let a = firing_of(&report, "first");
    let routing = seq_of(&report.state.log, |record| {
        matches!(
            &record.event,
            Event::RoutingResolved {
                decision_id: engine::DecisionId::Route { firing, .. },
                ..
            } if *firing == a
        )
    });
    let cut = routing + 1;
    assert_eq!(
        report.state.log.records()[cut].source,
        EventSource::Core,
        "the cut lands after the resolved decision but before its applied route"
    );
    let prefix = report.state.log.prefix(cut);

    let (driver, info) =
        resume_driver(graph.clone(), prefix, &dir, chain_registry(&first, &second));
    let suffix: Vec<u64> = info.log.records()[info.loaded..]
        .iter()
        .map(|r| r.seq)
        .collect();
    assert!(!suffix.is_empty(), "replay regenerated the lost tail");

    let observer = Arc::new(SeqObserver::default());
    let resumed = driver
        .observe(observer.clone() as Arc<dyn driver::EventObserver>)
        .await_run()
        .await;
    assert_eq!(resumed.status, RunStatus::Success);

    let seen = observer.seen.lock().expect("not poisoned");
    let seen_seqs: Vec<u64> = seen.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(
        &seen_seqs[..suffix.len()],
        suffix.as_slice(),
        "the regenerated suffix arrives before any new record"
    );
    assert!(
        seen.iter()
            .any(|(seq, source)| suffix.contains(seq) && *source == EventSource::Core),
        "Core records are in the notified suffix"
    );
    let mut sorted = seen_seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seen_seqs.len(), "each seq exactly once");
}

/// A crash during a retry backoff: resume re-arms the retry — and produced
/// *only* `ScheduleRetry` for that firing, never a `StartStep` that would
/// execute the step before its backoff.
#[tokio::test]
async fn a_pending_retry_is_rearmed_not_restarted() {
    let dir = RunDir::new("resume-retry");
    let runs = Arc::new(AtomicUsize::new(0));
    let mut b = GraphBuilder::new();
    let flaky = b.add_step("flaky", ScopeId::new(0), FLAKY);
    b.node_mut(flaky).retry = ir::RetryPolicy::attempts(2).with_backoff(ir::Backoff {
        initial: Duration::from_millis(50),
        factor:  1.0,
        max:     Duration::from_millis(200),
        jitter:  false,
    });
    let graph = b.build();

    let report = host_driver_full(
        graph.clone(),
        &dir,
        executor::MapSecrets::empty(),
        RunConfig::new(dir.path()),
        registry_with(Arc::new(FlakyStep { runs: runs.clone() })),
    )
    .await_run()
    .await;
    assert_eq!(report.status, RunStatus::Success);
    assert_eq!(runs.load(Ordering::SeqCst), 2);

    let elapsed = seq_of(&report.state.log, |r| {
        matches!(&r.event, Event::RetryElapsed { .. })
    });
    let prefix = report.state.log.prefix(elapsed);

    // The engine-level contract: only ScheduleRetry for the awaiting firing.
    let point = engine::resume(graph.clone(), &prefix).expect("resumes");
    let starts = point
        .pending
        .iter()
        .filter(|c| matches!(c, Command::StartStep(_)))
        .count();
    let retries = point
        .pending
        .iter()
        .filter(|c| matches!(c, Command::ScheduleRetry { .. }))
        .count();
    assert_eq!((starts, retries), (0, 1), "{:?}", point.pending);
    assert!(
        point.redispatched.is_empty(),
        "an awaiting firing is not re-dispatched"
    );

    let (driver, _info) = resume_driver(
        graph.clone(),
        prefix,
        &dir,
        registry_with(Arc::new(FlakyStep { runs: runs.clone() })),
    );
    let resumed = driver.await_run().await;
    assert_eq!(
        resumed.status,
        RunStatus::Success,
        "{:?}",
        resumed.state.errors()
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        3,
        "the retry attempt ran once more"
    );
    assert_replay_identical(&graph, &resumed);
}

/// A run stopped mid-step. With `kill`, the step ignores the polite cancel and
/// a second root cancel escalates to the kill tier.
async fn stopped_run(
    dir: &RunDir,
    kill: bool,
    runs: &Arc<AtomicUsize>,
) -> (Graph, ExecutionReport) {
    let marker = dir.path().join("marker");
    let mut b = GraphBuilder::new();
    b.add_node(
        "work",
        ScopeId::new(0),
        StepRef::new(WAITING, json!({ "marker": marker.to_string_lossy() })),
    );
    let graph = b.build();

    let driver = host_driver_full(
        graph.clone(),
        dir,
        executor::MapSecrets::empty(),
        RunConfig::new(dir.path()),
        registry_with(Arc::new(WaitingStep {
            runs: runs.clone(),
            hard: kill,
        })),
    );
    let handle: RunHandle = driver.handle();
    let run = tokio::spawn(driver.run());
    assert!(
        wait_for_file(&marker, Duration::from_secs(10)).await,
        "the step never started"
    );
    handle.cancel(ir::CancelScopeId::ROOT).await;
    if kill {
        handle.cancel(ir::CancelScopeId::ROOT).await;
    }
    let report = run.await.expect("the run task");
    assert_eq!(report.status, RunStatus::Cancelled);
    (graph, report)
}

/// A kill in flight: the log ends after `KillRequested` with the firing still
/// live. Resume never re-spawns it — the finish carries
/// `killed_before_resume`, records without routing, and the run quiesces
/// `Cancelled`.
#[tokio::test]
async fn a_killed_firing_is_finished_not_respawned() {
    let dir = RunDir::new("resume-killed");
    let runs = Arc::new(AtomicUsize::new(0));
    let (graph, report) = stopped_run(&dir, true, &runs).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    let target = firing_of(&report, "work");
    let cut = finish_seq(&report.state.log, target);
    let prefix = report.state.log.prefix(cut);
    assert!(
        prefix
            .events()
            .any(|e| matches!(e, Event::KillRequested { .. })),
        "the kill tier is in the loaded log"
    );

    let resumed_runs = Arc::new(AtomicUsize::new(0));
    let (driver, info) = resume_driver(
        graph.clone(),
        prefix,
        &dir,
        registry_with(Arc::new(WaitingStep {
            runs: resumed_runs.clone(),
            hard: true,
        })),
    );
    assert!(
        info.redispatched.is_empty(),
        "a cancelling firing is not re-dispatched"
    );
    let resumed = driver.await_run().await;

    assert_eq!(resumed.status, RunStatus::Cancelled);
    assert_eq!(resumed_runs.load(Ordering::SeqCst), 0, "never re-spawned");
    assert_eq!(
        escalation_of(&resumed, "work").as_deref(),
        Some(KILLED_BEFORE_RESUME.as_str())
    );
    assert_replay_identical(&graph, &resumed);
}

/// The polite tier, truncated right after the root `CancelRequested`: the
/// firing is cancelling, resume finishes it with `cancelled_before_resume`,
/// and the run drives to a quiescent `Cancelled` report.
#[tokio::test]
async fn a_cancel_truncation_drives_to_a_quiescent_cancelled_report() {
    let dir = RunDir::new("resume-cancelled");
    let runs = Arc::new(AtomicUsize::new(0));
    let (graph, report) = stopped_run(&dir, false, &runs).await;

    let cut = seq_of(&report.state.log, |r| {
        matches!(&r.event, Event::CancelRequested { .. })
    }) + 1;
    let prefix = report.state.log.prefix(cut);

    let resumed_runs = Arc::new(AtomicUsize::new(0));
    let (driver, info) = resume_driver(
        graph.clone(),
        prefix,
        &dir,
        registry_with(Arc::new(WaitingStep {
            runs: resumed_runs.clone(),
            hard: false,
        })),
    );
    assert!(info.redispatched.is_empty());
    let resumed = driver.await_run().await;

    assert_eq!(resumed.status, RunStatus::Cancelled);
    assert_eq!(resumed_runs.load(Ordering::SeqCst), 0, "not re-spawned");
    assert_eq!(
        escalation_of(&resumed, "work").as_deref(),
        Some(CANCELLED_BEFORE_RESUME.as_str())
    );
    assert_replay_identical(&graph, &resumed);
}

/// The polite outcome routes exactly as a live cancel's would: a downstream
/// `run_on_cancel` cleanup node fires for real after the synthesized finish —
/// and a cleanup firing that was itself live at the crash re-dispatches
/// normally.
#[tokio::test]
async fn the_synthesized_cancel_routes_and_cleanup_redispatches() {
    let dir = RunDir::new("resume-cleanup");
    let marker = dir.path().join("marker");
    let work_runs = Arc::new(AtomicUsize::new(0));
    let cleanup_runs = Arc::new(AtomicUsize::new(0));

    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let work = b.add_node(
        "work",
        scope,
        StepRef::new(WAITING, json!({ "marker": marker.to_string_lossy() })),
    );
    let cleanup = b.add_step("cleanup", scope, COUNTING);
    b.node_mut(cleanup).run_on_cancel = true;
    b.link(work, cleanup);
    let graph = b.build();

    let registry = |work_counter: &Arc<AtomicUsize>, cleanup_counter: &Arc<AtomicUsize>| {
        let mut registry = runners();
        registry.register_runner(Arc::new(WaitingStep {
            runs: work_counter.clone(),
            hard: false,
        }));
        registry.register_runner(Arc::new(CountingStep {
            runs: cleanup_counter.clone(),
        }));
        registry
    };

    let driver = host_driver_full(
        graph.clone(),
        &dir,
        executor::MapSecrets::empty(),
        RunConfig::new(dir.path()),
        registry(&work_runs, &cleanup_runs),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    assert!(wait_for_file(&marker, Duration::from_secs(10)).await);
    handle.cancel(ir::CancelScopeId::ROOT).await;
    let report = run.await.expect("the run task");
    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(cleanup_runs.load(Ordering::SeqCst), 1, "cleanup ran live");

    // Crash A: work still cancelling. The synthesized finish routes; cleanup
    // fires fresh, admitted through run_on_cancel.
    let work_firing = firing_of(&report, "work");
    let prefix = report
        .state
        .log
        .prefix(finish_seq(&report.state.log, work_firing));
    let (driver, _info) = resume_driver(
        graph.clone(),
        prefix,
        &dir,
        registry(&work_runs, &cleanup_runs),
    );
    let resumed = driver.await_run().await;
    assert_eq!(resumed.status, RunStatus::Cancelled);
    assert_eq!(
        escalation_of(&resumed, "work").as_deref(),
        Some(CANCELLED_BEFORE_RESUME.as_str())
    );
    assert_eq!(status_of(&resumed, "cleanup").as_deref(), Some("success"));
    assert_eq!(
        cleanup_runs.load(Ordering::SeqCst),
        2,
        "cleanup ran again, for real"
    );
    assert_replay_identical(&graph, &resumed);

    // Crash B: cleanup itself was live. An ordinary re-dispatch — it fired
    // after the cancel, so it is not cancelling.
    let cleanup_firing = firing_of(&report, "cleanup");
    let prefix = report
        .state
        .log
        .prefix(finish_seq(&report.state.log, cleanup_firing));
    let (driver, info) = resume_driver(
        graph.clone(),
        prefix,
        &dir,
        registry(&work_runs, &cleanup_runs),
    );
    assert_eq!(info.redispatched, vec![cleanup_firing]);
    let resumed = driver.await_run().await;
    assert_eq!(resumed.status, RunStatus::Cancelled);
    assert_eq!(
        cleanup_runs.load(Ordering::SeqCst),
        3,
        "the live cleanup re-ran"
    );
    assert_replay_identical(&graph, &resumed);
}

/// An executor stub that counts acquisitions and delegates to the host.
struct CountingExecutor {
    inner:    executor_sandbox::HostExecutor,
    acquires: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl executor::Executor for CountingExecutor {
    async fn acquire(
        &self,
        scope: &executor::ScopeSpec,
        ctx: &executor::AcquireContext,
    ) -> Result<executor::EnvHandle, executor::EnvError> {
        self.acquires.fetch_add(1, Ordering::SeqCst);
        self.inner.acquire(scope, ctx).await
    }

    async fn release(
        &self,
        env: executor::EnvHandle,
        outcome: executor::ScopeOutcome,
    ) -> executor::ReleaseReport {
        self.inner.release(env, outcome).await
    }
}

/// A held scope is re-acquired on resume.
#[tokio::test]
async fn resume_reacquires_held_scopes() {
    let dir = RunDir::new("resume-acquire");
    let (graph, first, second) = counting_chain();
    let report = run_chain(&graph, &dir, &first, &second).await;

    let target = firing_of(&report, "second");
    let prefix = report
        .state
        .log
        .prefix(finish_seq(&report.state.log, target));

    let acquires = Arc::new(AtomicUsize::new(0));
    let executor: Arc<dyn executor::Executor> = Arc::new(CountingExecutor {
        inner:    executor_sandbox::HostExecutor::new(dir.path()),
        acquires: acquires.clone(),
    });
    let (driver, _info) = Driver::resume(
        graph.clone(),
        prefix,
        executor,
        chain_registry(&first, &second),
        Arc::new(executor::MapSecrets::empty()),
        RunConfig::new(dir.path()),
    )
    .expect("resumes");
    let resumed = driver.await_run().await;
    assert_eq!(resumed.status, RunStatus::Success);
    assert_eq!(
        acquires.load(Ordering::SeqCst),
        1,
        "the held scope was re-acquired"
    );
}

/// The flagship shape: a human gate waits on `ctx.control`, the process dies,
/// resume brings the gate back up — with **no automatic re-delivery**, the
/// logged `ControlRequested` notwithstanding — and the host's re-sent answer
/// completes the run.
#[tokio::test]
async fn a_gate_resumes_waiting_and_the_host_redelivers() {
    let dir = RunDir::new("resume-gate");
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut b = GraphBuilder::new();
    b.add_step("gate", ScopeId::new(0), GATE_KIND);
    let graph = b.build();
    let gate_firing = FiringId::new(1);

    let driver = host_driver_full(
        graph.clone(),
        &dir,
        executor::MapSecrets::empty(),
        RunConfig::new(dir.path()),
        registry_with(Arc::new(GateStep {
            received: received.clone(),
        })),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    assert_eq!(
        handle
            .deliver(gate_firing, Control::Deliver(json!("yes")))
            .await,
        driver::DeliverDisposition::Delivered
    );
    let report = run.await.expect("the run task");
    assert_eq!(report.status, RunStatus::Success);

    let prefix = report
        .state
        .log
        .prefix(finish_seq(&report.state.log, gate_firing));
    assert!(
        prefix
            .events()
            .any(|e| matches!(e, Event::ControlRequested { .. })),
        "the question is in the loaded log"
    );

    let resumed_received = Arc::new(Mutex::new(Vec::new()));
    let (driver, info) = resume_driver(
        graph.clone(),
        prefix,
        &dir,
        registry_with(Arc::new(GateStep {
            received: resumed_received.clone(),
        })),
    );
    assert_eq!(info.redispatched, vec![gate_firing]);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    // No automatic re-delivery: the resumed gate waits again.
    time::sleep(Duration::from_millis(200)).await;
    assert!(
        resumed_received.lock().expect("not poisoned").is_empty(),
        "the logged answer was not re-forwarded"
    );
    assert!(!run.is_finished(), "the gate is waiting");

    assert_eq!(
        handle
            .deliver(gate_firing, Control::Deliver(json!("again")))
            .await,
        driver::DeliverDisposition::Delivered
    );
    let resumed = run.await.expect("the run task");
    assert_eq!(resumed.status, RunStatus::Success);
    assert_eq!(output_of(&resumed, "gate"), json!("again"));
    assert_eq!(
        *resumed_received.lock().expect("not poisoned"),
        vec![json!("again")],
        "exactly one delivery reached the resumed step"
    );
    assert_replay_identical(&graph, &resumed);
}

const SECRET: &str = "resume-secret-3f9a2b7c1d4e";

/// Dynamic secrets are not in the log by design: re-registered they resolve;
/// missing, the delivery fails the step with `secret_unavailable`.
#[tokio::test]
async fn a_dynamic_secret_must_be_reregistered_after_resume() {
    let dir = RunDir::new("resume-secret");
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut b = GraphBuilder::new();
    b.add_step("gate", ScopeId::new(0), GATE_KIND);
    let graph = b.build();
    let gate_firing = FiringId::new(1);

    let secrets = Arc::new(executor::MapSecrets::empty());
    let driver = host_driver_shared(
        graph.clone(),
        &dir,
        secrets.clone(),
        RunConfig::new(dir.path()),
        registry_with(Arc::new(GateStep {
            received: received.clone(),
        })),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    secrets.register("answer:1", SECRET).expect("registered");
    handle
        .deliver(
            gate_firing,
            Control::Deliver(json!({ "$secret": "answer:1" })),
        )
        .await;
    let report = run.await.expect("the run task");
    assert_eq!(report.status, RunStatus::Success);

    let prefix = report
        .state
        .log
        .prefix(finish_seq(&report.state.log, gate_firing));

    // Not re-registered: the reference cannot resolve; the step fails routably.
    let (driver, _info) = resume_driver(
        graph.clone(),
        prefix.clone(),
        &dir,
        registry_with(Arc::new(GateStep {
            received: Arc::new(Mutex::new(Vec::new())),
        })),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    time::sleep(Duration::from_millis(100)).await;
    handle
        .deliver(
            gate_firing,
            Control::Deliver(json!({ "$secret": "answer:1" })),
        )
        .await;
    let resumed = run.await.expect("the run task");
    assert_eq!(resumed.status, RunStatus::Failed);
    let record = resumed
        .state
        .history()
        .iter()
        .find(|r| r.name == "gate")
        .expect("recorded");
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some(steps::SECRET_UNAVAILABLE_CLASS.as_str())
    );

    // Re-registered: the same reference resolves, and the log stays clean.
    let resumed_received = Arc::new(Mutex::new(Vec::new()));
    let fresh = Arc::new(executor::MapSecrets::empty());
    fresh.register("answer:1", SECRET).expect("registered");
    let (driver, _info) = resume_driver_shared(
        graph.clone(),
        prefix,
        &dir,
        fresh,
        registry_with(Arc::new(GateStep {
            received: resumed_received.clone(),
        })),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    time::sleep(Duration::from_millis(100)).await;
    handle
        .deliver(
            gate_firing,
            Control::Deliver(json!({ "$secret": "answer:1" })),
        )
        .await;
    let resumed = run.await.expect("the run task");
    assert_eq!(resumed.status, RunStatus::Success);
    assert_eq!(
        *resumed_received.lock().expect("not poisoned"),
        vec![json!(SECRET)],
        "the resumed step saw the real value"
    );
    let log_bytes = serde_json::to_string(&resumed.state.log).expect("encode");
    assert!(!log_bytes.contains(SECRET), "the log keeps the reference");
}

/// A tampered record — decodable, but not what the run produced — refuses to
/// resume with `ReplayMismatch`.
#[tokio::test]
async fn a_tampered_record_refuses_to_resume() {
    let dir = RunDir::new("resume-tamper");
    let (graph, first, second) = counting_chain();
    let report = run_chain(&graph, &dir, &first, &second).await;

    let mut encoded = serde_json::to_value(&report.state.log).expect("encodes");
    let records = encoded["records"].as_array_mut().expect("records");
    let token = records
        .iter_mut()
        .find(|r| r["event"].get("TokenEmitted").is_some())
        .expect("a routed token");
    token["event"]["TokenEmitted"]["payload"] = json!("tampered");
    let tampered: EventLog = serde_json::from_value(encoded).expect("still decodes");

    let executor: Arc<dyn executor::Executor> =
        Arc::new(executor_sandbox::HostExecutor::new(dir.path()));
    let result = Driver::resume(
        graph,
        tampered,
        executor,
        chain_registry(&first, &second),
        Arc::new(executor::MapSecrets::empty()),
        RunConfig::new(dir.path()),
    );
    assert!(
        matches!(result, Err(ResumeError::Replay(_))),
        "divergence is refused, not accepted as a crash prefix"
    );
}

/// Rewind: truncate at an earlier finish, change what the step will do, resume
/// — the continuation routes differently and still verifies.
#[tokio::test]
async fn a_rewound_run_diverges_and_still_verifies() {
    let dir = RunDir::new("resume-rewind");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let decide = add_script(
        &mut b,
        "decide",
        scope,
        r#"out=$(cat choice 2>/dev/null || echo A); echo "out=$out" > "$CI_OUTPUT""#,
    );
    let left = add_script(&mut b, "left", scope, "echo took-left");
    let right = add_script(&mut b, "right", scope, "echo took-right");
    let took_a = {
        let e = b.exprs();
        let out = e.path("output", &["out"]);
        let a = e.lit("A");
        e.binary(BinOp::Eq, out, a)
    };
    b.select(decide, vec![Arm::when(left, took_a), Arm::always(right)]);
    let graph = b.build();

    let config = RunConfig::new(dir.path()).with_retention(RETAIN);
    let report = host_driver_with(
        graph.clone(),
        &dir,
        executor::MapSecrets::empty(),
        config.clone(),
    )
    .await_run()
    .await;
    assert_eq!(report.status, RunStatus::Success);
    assert_eq!(status_of(&report, "left").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "right"), None);

    // Rewind to before `decide` finished; the world has changed since.
    let decide_firing = firing_of(&report, "decide");
    let prefix = report
        .state
        .log
        .prefix(finish_seq(&report.state.log, decide_firing));
    fs::write(dir.workspace().join("choice"), b"B").expect("the changed world");

    let executor: Arc<dyn executor::Executor> =
        Arc::new(executor_sandbox::HostExecutor::new(dir.path()).with_retention(RETAIN));
    let (driver, _info) = Driver::resume(
        graph.clone(),
        prefix,
        executor,
        runners(),
        Arc::new(executor::MapSecrets::empty()),
        config,
    )
    .expect("resumes");
    let resumed = driver.await_run().await;
    assert_eq!(
        resumed.status,
        RunStatus::Success,
        "{:?}",
        resumed.state.errors()
    );
    assert_eq!(status_of(&resumed, "right").as_deref(), Some("success"));
    assert_eq!(
        status_of(&resumed, "left"),
        None,
        "the continuation diverged"
    );
    assert_replay_identical(&graph, &resumed);
}
