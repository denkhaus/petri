//! Driver-specific scaffolding over the shared [`testkit`]: building drivers with
//! particular executors, registries and failure modes.

#![allow(dead_code)]

pub use testkit::*;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use driver::{Driver, RunConfig, RunReport};
use engine::{Event, EventLog, EventRecord};
use executor::{Executor, MapSecrets, Retention};
use executor_docker::DockerExecutor;
use executor_host::HostExecutor;
use ir::{Control, FiringId, Graph, Outcome, ScopeId, Value};
use steps::{ProcessStep, Registry};

// ── Helpers over the log and report ───────────────────────────────────────

/// The log position of the first record matching `pred`.
pub fn seq_of(log: &EventLog, pred: impl Fn(&EventRecord) -> bool) -> usize {
    log.records()
        .iter()
        .find(|r| pred(r))
        .map(|r| r.seq as usize)
        .expect("the record is in the log")
}

/// The log position of a firing's `StepFinished` record.
pub fn finish_seq(log: &EventLog, firing: FiringId) -> usize {
    seq_of(
        log,
        |r| matches!(&r.event, Event::StepFinished { firing: f, .. } if *f == firing),
    )
}

/// The firing recorded for the named node.
pub fn firing_of(report: &RunReport, name: &str) -> FiringId {
    report
        .state
        .history()
        .iter()
        .find(|r| r.name == name)
        .expect("the node recorded")
        .firing
}

pub fn runners() -> Registry {
    let mut registry = Registry::new();
    registry.register(ProcessStep);
    registry
}

/// Build a driver over the host executor.
pub fn host_driver(graph: Graph, dir: &RunDir) -> Driver {
    host_driver_with(graph, dir, MapSecrets::empty(), RunConfig::new(dir.path()))
}

pub fn host_driver_with(
    graph: Graph,
    dir: &RunDir,
    secrets: MapSecrets,
    config: RunConfig,
) -> Driver {
    host_driver_full(graph, dir, secrets, config, runners())
}

pub fn host_driver_full(
    graph: Graph,
    dir: &RunDir,
    secrets: MapSecrets,
    config: RunConfig,
    runners: Registry,
) -> Driver {
    host_driver_shared(graph, dir, Arc::new(secrets), config, runners)
}

/// Like [`host_driver_full`], but the caller keeps a handle to the secrets — to
/// register values mid-run.
pub fn host_driver_shared(
    graph: Graph,
    dir: &RunDir,
    secrets: Arc<MapSecrets>,
    config: RunConfig,
    runners: Registry,
) -> Driver {
    let executor: Arc<dyn Executor> =
        Arc::new(HostExecutor::new(dir.path()).with_retention(config.keep_workspaces));
    Driver::new(graph, executor, runners, secrets, config)
}

/// A host executor that refuses to acquire particular scopes, standing in for one
/// bad image among several jobs without needing a Docker daemon.
pub struct SelectivelyBroken {
    inner: HostExecutor,
    broken: std::collections::HashSet<ScopeId>,
    message: String,
}

#[async_trait::async_trait]
impl Executor for SelectivelyBroken {
    async fn acquire(
        &self,
        scope: &executor::ScopeSpec,
        ctx: &executor::AcquireContext,
    ) -> Result<executor::EnvHandle, executor::EnvError> {
        if self.broken.contains(&scope.id) {
            return Err(executor::EnvError::Backend {
                backend: smol_str::SmolStr::new("test"),
                operation: smol_str::SmolStr::new("acquire"),
                message: self.message.clone(),
            });
        }
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

/// A driver whose executor cannot acquire `broken`, but is otherwise a host executor.
pub fn broken_scope_driver(
    graph: Graph,
    dir: &RunDir,
    broken: &[ScopeId],
    message: &str,
) -> Driver {
    let executor: Arc<dyn Executor> = Arc::new(SelectivelyBroken {
        inner: HostExecutor::new(dir.path()).with_retention(Retention::Never),
        broken: broken.iter().copied().collect(),
        message: message.to_string(),
    });
    Driver::new(
        graph,
        executor,
        runners(),
        Arc::new(MapSecrets::empty()),
        RunConfig::new(dir.path()),
    )
}

pub fn runners_with_wedged() -> Registry {
    let mut registry = runners();
    registry.register_runner(Arc::new(WedgedStep));
    registry
}

/// Build a driver over the Docker executor, and the container-name prefix it will
/// use, so a leak check can look only at this test's own containers.
pub async fn docker_driver_named(
    graph: Graph,
    dir: &RunDir,
    config: RunConfig,
) -> (Driver, String) {
    let executor = DockerExecutor::new(dir.path()).with_retention(config.keep_workspaces);
    let prefix = executor
        .container_prefix()
        .await
        .expect("the run id is recorded in the run dir");
    let driver = Driver::new(
        graph,
        Arc::new(executor),
        runners(),
        Arc::new(MapSecrets::empty()),
        config,
    );
    (driver, prefix)
}

pub fn docker_driver(graph: Graph, dir: &RunDir, config: RunConfig) -> Driver {
    let executor: Arc<dyn Executor> =
        Arc::new(DockerExecutor::new(dir.path()).with_retention(config.keep_workspaces));
    Driver::new(
        graph,
        executor,
        runners(),
        Arc::new(MapSecrets::empty()),
        config,
    )
}

/// Run a graph on the host executor and return the report.
pub async fn run_host(graph: Graph, dir: &RunDir) -> RunReport {
    host_driver(graph, dir).await_run().await
}

/// Convenience so tests read as `driver.await_run()`.
pub trait DriverExt {
    fn await_run(self) -> std::pin::Pin<Box<dyn std::future::Future<Output = RunReport> + Send>>;
}

impl DriverExt for Driver {
    fn await_run(self) -> std::pin::Pin<Box<dyn std::future::Future<Output = RunReport> + Send>> {
        Box::pin(self.run())
    }
}

/// A step kind that spawns a real process tree and then wedges: it ignores
/// `Control::Cancel` and never returns, so the driver's hard deadline aborts its
/// future — and, with `kill_on_drop` gone, only scope release can end the tree.
pub struct SpawnAndWedge;

pub const SPAWN_AND_WEDGE_KIND: ir::StepKindId = ir::StepKindId::new_static("spawn-and-wedge");

impl ir::StepKind for SpawnAndWedge {
    fn id(&self) -> ir::StepKindId {
        SPAWN_AND_WEDGE_KIND
    }

    fn name(&self) -> &str {
        "spawn-and-wedge"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for SpawnAndWedge {
    async fn run(&self, mut ctx: steps::StepCtx) -> ir::Outcome {
        let spec = executor::ProcessSpec::new(
            "bash",
            &[
                "-c",
                "( while :; do echo tick >> heartbeat; sleep 0.05; done ) >/dev/null 2>&1 & \
                 echo ready > ready; sleep 300",
            ],
        );
        let _handle = ctx.env.spawn(spec).await.expect("spawn");
        let _ = ctx.control.recv().await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }
}

pub fn runners_with_spawn_and_wedge() -> Registry {
    let mut registry = runners();
    registry.register_runner(Arc::new(SpawnAndWedge));
    registry
}

/// The minimal human-gate shape: wait for one `Deliver`, record what arrived, and
/// return it as the step's output. An optional `linger_ms` config keeps it running
/// that long after the answer before returning it.
pub struct GateStep {
    pub received: Arc<Mutex<Vec<Value>>>,
}

pub const GATE_KIND: ir::StepKindId = ir::StepKindId::new_static("gate");

impl ir::StepKind for GateStep {
    fn id(&self) -> ir::StepKindId {
        GATE_KIND
    }

    fn name(&self) -> &str {
        "gate"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for GateStep {
    async fn run(&self, mut ctx: steps::StepCtx) -> Outcome {
        match ctx.control.recv().await {
            Some(Control::Deliver(value)) => {
                self.received
                    .lock()
                    .expect("not poisoned")
                    .push(value.clone());
                let linger = ctx.config["linger_ms"].as_u64().unwrap_or(0);
                if linger > 0 {
                    tokio::time::sleep(Duration::from_millis(linger)).await;
                }
                Outcome::success(value)
            }
            Some(_) => Outcome::cancelled(),
            None => Outcome::failure("the control channel closed with no answer"),
        }
    }
}
