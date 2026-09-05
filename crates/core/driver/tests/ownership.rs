//! Abandoning a driver must stop its tasks before releasing their resources.

mod support;

use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use driver::{Driver, RunConfig, RunGuard};
use executor::{Executor, MapSecrets, Retention};
use executor_sandbox::HostExecutor;
use ir::{CancelScopeId, GraphBuilder, Outcome, RunStatus, ScopeId, StepKindId};
use steps::{Registry, StepCtx, StepRunner};
use support::*;
use tokio::sync::{Notify, oneshot};
use tokio::time;

const OWNED_KIND: StepKindId = StepKindId::new_static("owned");
const WAIT: Duration = Duration::from_secs(10);

struct BurstLogs;

#[async_trait::async_trait]
impl steps::Step for BurstLogs {
    const NAME: &'static str = "burst-logs";
    type Config = ir::Value;

    async fn run(&self, _config: ir::Value, ctx: StepCtx) -> Outcome {
        for index in 0..128 {
            ctx.logs
                .try_send(ir::StepEvent::Log {
                    stream: ir::LogStream::Stdout,
                    line:   format!("line {index}"),
                })
                .expect("the burst fits in the log channel");
        }
        Outcome::success(ir::Value::Null)
    }
}

#[tokio::test]
async fn a_runner_returning_after_a_burst_preserves_every_log_event() {
    let dir = RunDir::new("driver-burst-logs");
    let mut runners = Registry::new();
    let kind = runners.register(BurstLogs);
    let mut graph = GraphBuilder::new();
    graph.add_step("burst", ScopeId::new(0), kind);
    let report = host_driver_full(
        graph.build(),
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()),
        runners,
    )
    .run()
    .await;
    assert_eq!(report.status, RunStatus::Success);
    assert_eq!(
        log_lines(&report),
        (0..128)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
    );
}

#[derive(Default)]
struct Lifecycle {
    started:         Notify,
    dropped:         AtomicBool,
    release_started: Notify,
    released:        AtomicBool,
}

struct StepLifetime(Arc<Lifecycle>);

impl Drop for StepLifetime {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}

struct OwnedStep(Arc<Lifecycle>);

impl ir::StepKind for OwnedStep {
    fn id(&self) -> StepKindId {
        OWNED_KIND
    }

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the trait fixes the signature"
    )]
    fn name(&self) -> &str {
        "owned"
    }
}

#[async_trait::async_trait]
impl StepRunner for OwnedStep {
    async fn run(&self, _ctx: StepCtx) -> Outcome {
        let _lifetime = StepLifetime(self.0.clone());
        self.0.started.notify_one();
        pending().await
    }
}

struct ObservedExecutor {
    inner:        HostExecutor,
    lifecycle:    Arc<Lifecycle>,
    release_gate: Option<Arc<Notify>>,
}

#[async_trait::async_trait]
impl Executor for ObservedExecutor {
    async fn acquire(
        &self,
        scope: &executor::ScopeSpec,
        ctx: &executor::AcquireContext,
    ) -> Result<executor::EnvHandle, executor::EnvError> {
        self.inner.acquire(scope, ctx).await
    }

    async fn release(
        &self,
        env: executor::EnvHandle,
        outcome: executor::ScopeOutcome,
    ) -> executor::ReleaseReport {
        assert!(
            self.lifecycle.dropped.load(Ordering::SeqCst),
            "the runner must be dropped before scope release starts"
        );
        self.lifecycle.release_started.notify_one();
        if let Some(gate) = &self.release_gate {
            gate.notified().await;
        }
        let report = self.inner.release(env, outcome).await;
        assert!(report.is_clean(), "{report:?}");
        self.lifecycle.released.store(true, Ordering::SeqCst);
        report
    }
}

struct FinishedGuard {
    lifecycle: Arc<Lifecycle>,
    done:      oneshot::Sender<()>,
}

#[async_trait::async_trait]
impl RunGuard for FinishedGuard {
    async fn teardown(self: Box<Self>) {
        assert!(
            self.lifecycle.released.load(Ordering::SeqCst),
            "the scope must be released before the run's services stop"
        );
        let _ = self.done.send(());
    }
}

fn owned_driver(
    dir: &RunDir,
    lifecycle: &Arc<Lifecycle>,
    release_gate: Option<Arc<Notify>>,
) -> (Driver, oneshot::Receiver<()>) {
    let mut graph = GraphBuilder::new();
    graph.add_step("owned", ScopeId::new(0), OWNED_KIND);
    let mut runners = Registry::new();
    runners.register_runner(Arc::new(OwnedStep(lifecycle.clone())));
    let executor = Arc::new(ObservedExecutor {
        inner: HostExecutor::new(dir.path()).with_retention(Retention::Never),
        lifecycle: lifecycle.clone(),
        release_gate,
    });
    let (done, finished) = oneshot::channel();
    let driver = Driver::new(
        graph.build(),
        executor,
        runners,
        Arc::new(MapSecrets::empty()),
        RunConfig {
            hard_deadline_slack: Duration::ZERO,
            ..RunConfig::new(dir.path()).with_grace(Duration::ZERO)
        },
    )
    .with_run_guard(Box::new(FinishedGuard {
        lifecycle: lifecycle.clone(),
        done,
    }));
    (driver, finished)
}

#[tokio::test]
async fn aborting_the_run_joins_the_runner_and_releases_its_scope() {
    let dir = RunDir::new("abort-driver-runner");
    let lifecycle = Arc::new(Lifecycle::default());
    let (driver, finished) = owned_driver(&dir, &lifecycle, None);
    let run = tokio::spawn(driver.run());
    time::timeout(WAIT, lifecycle.started.notified())
        .await
        .expect("the runner started");

    run.abort();
    assert!(matches!(run.await, Err(error) if error.is_cancelled()));
    time::timeout(WAIT, finished)
        .await
        .expect("driver cleanup finished")
        .expect("guard teardown was awaited");
    assert!(!dir.workspace().exists(), "the scope was released");
}

#[tokio::test]
async fn dropping_the_run_future_joins_the_runner_and_releases_its_scope() {
    let dir = RunDir::new("drop-driver-future");
    let lifecycle = Arc::new(Lifecycle::default());
    let (driver, finished) = owned_driver(&dir, &lifecycle, None);
    let mut run = Box::pin(driver.run());
    time::timeout(WAIT, async {
        tokio::select! {
            () = lifecycle.started.notified() => {},
            _ = &mut run => panic!("the runner cannot finish by itself"),
        }
    })
    .await
    .expect("the runner started");

    drop(run);
    time::timeout(WAIT, finished)
        .await
        .expect("driver cleanup finished")
        .expect("guard teardown was awaited");
}

#[tokio::test]
async fn forced_finish_joins_the_runner_before_scope_release() {
    let dir = RunDir::new("finish-driver-runner");
    let lifecycle = Arc::new(Lifecycle::default());
    let (driver, finished) = owned_driver(&dir, &lifecycle, None);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    time::timeout(WAIT, lifecycle.started.notified())
        .await
        .expect("the runner started");

    handle.cancel(CancelScopeId::ROOT).await;
    let report = time::timeout(WAIT, run)
        .await
        .expect("the hard deadline ends the run")
        .expect("the driver finished");
    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(
        report
            .releases
            .iter()
            .all(executor::ReleaseReport::is_clean)
    );
    finished.await.expect("guard teardown preceded the report");
}

#[tokio::test]
async fn aborting_during_release_keeps_services_until_release_finishes() {
    let dir = RunDir::new("abort-driver-release");
    let lifecycle = Arc::new(Lifecycle::default());
    let gate = Arc::new(Notify::new());
    let (driver, mut finished) = owned_driver(&dir, &lifecycle, Some(gate.clone()));
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    time::timeout(WAIT, lifecycle.started.notified())
        .await
        .expect("the runner started");
    handle.cancel(CancelScopeId::ROOT).await;
    time::timeout(WAIT, lifecycle.release_started.notified())
        .await
        .expect("scope release started");

    run.abort();
    assert!(matches!(run.await, Err(error) if error.is_cancelled()));
    assert!(
        time::timeout(Duration::from_millis(50), &mut finished)
            .await
            .is_err(),
        "guard teardown waits for the in-flight release"
    );
    gate.notify_one();
    time::timeout(WAIT, finished)
        .await
        .expect("driver cleanup finished")
        .expect("guard teardown was awaited");
}

#[tokio::test]
async fn aborting_the_driver_stops_the_steps_process_tree() {
    let dir = RunDir::new("abort-driver-process-tree");
    let mut graph = GraphBuilder::new();
    graph.add_step("wedged", ScopeId::new(0), SPAWN_AND_WEDGE_KIND);
    let driver = host_driver_full(
        graph.build(),
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()).with_retention(Retention::Never),
        runners_with_spawn_and_wedge(),
    );
    let run = tokio::spawn(driver.run());
    assert!(wait_for_file(&dir.workspace().join("ready"), WAIT).await);
    let heartbeat = dir.workspace().join("heartbeat");
    assert!(wait_for_file(&heartbeat, WAIT).await);

    run.abort();
    assert!(matches!(run.await, Err(error) if error.is_cancelled()));
    time::timeout(WAIT, async {
        while dir.workspace().exists() {
            time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("scope release stopped the process tree and removed its workspace");
}
