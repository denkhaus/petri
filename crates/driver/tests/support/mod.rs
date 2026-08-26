//! Scaffolding for end-to-end runs against real processes.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use driver::{Driver, RunConfig, RunReport};
use executor::{Executor, MapSecrets, Retention};
use executor_docker::DockerExecutor;
use executor_host::HostExecutor;
use ir::{Graph, GraphBuilder, NodeId, ScopeId, StepRef, Value};
use serde_json::json;
use steps::{PROCESS_KIND, ProcessStep, RunnerRegistry};

/// A run directory that cleans itself up.
pub struct RunDir {
    path: PathBuf,
}

impl RunDir {
    pub fn new(label: &str) -> Self {
        let unique = format!("{label}-{}-{}", std::process::id(), next_id());
        let path = std::env::temp_dir().join("petri-tests").join(unique);
        std::fs::create_dir_all(&path).expect("run dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The workspace the host executor gives scope 0.
    pub fn workspace(&self) -> PathBuf {
        self.workspace_of(ScopeId::new(0))
    }

    pub fn workspace_of(&self, scope: ScopeId) -> PathBuf {
        self.path
            .join("scopes")
            .join(format!("scope-{}", scope.raw()))
            .join("work")
    }

    pub fn logs(&self) -> PathBuf {
        self.path.join("logs")
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// A `run:` script step config.
pub fn script(run: &str) -> Value {
    json!({ "run": run })
}

pub fn script_with(run: &str, extra: Value) -> Value {
    let mut config = json!({ "run": run });
    if let (Some(base), Some(extra)) = (config.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    config
}

/// Add a process node running `run`.
pub fn add_script(b: &mut GraphBuilder, name: &str, scope: ScopeId, run: &str) -> NodeId {
    b.add_node(name, scope, StepRef::new(PROCESS_KIND, script(run)))
}

pub fn runners() -> RunnerRegistry {
    let mut registry = RunnerRegistry::new();
    registry.register(Arc::new(ProcessStep));
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
    runners: RunnerRegistry,
) -> Driver {
    let executor: Arc<dyn Executor> =
        Arc::new(HostExecutor::new(dir.path()).with_retention(config.keep_workspaces));
    Driver::new(graph, executor, runners, Arc::new(secrets), config)
}

/// A step kind that ignores `Control::Cancel` and never returns, so the driver's
/// hard deadline is the only thing that can end it.
pub struct WedgedStep;

pub const WEDGED_KIND: ir::StepKindId = ir::StepKindId::new(99);

#[async_trait::async_trait]
impl steps::StepRunner for WedgedStep {
    fn kind(&self) -> ir::StepKindId {
        WEDGED_KIND
    }

    fn name(&self) -> &str {
        "wedged"
    }

    async fn run(&self, mut ctx: steps::StepCtx) -> ir::Outcome {
        ctx.log(ir::LogStream::Stdout, "wedged step is running")
            .await;
        // Receive the cancel and deliberately do nothing about it.
        let _ = ctx.control.recv().await;
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
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
    ) -> Result<executor::EnvHandle, executor::EnvError> {
        if self.broken.contains(&scope.id) {
            return Err(executor::EnvError::Backend {
                backend: smol_str::SmolStr::new("test"),
                operation: smol_str::SmolStr::new("acquire"),
                message: self.message.clone(),
            });
        }
        self.inner.acquire(scope).await
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

pub fn runners_with_wedged() -> RunnerRegistry {
    let mut registry = runners();
    registry.register(Arc::new(WedgedStep));
    registry
}

/// Build a driver over the Docker executor, and the container-name prefix it will
/// use, so a leak check can look only at this test's own containers.
pub fn docker_driver_named(graph: Graph, dir: &RunDir, config: RunConfig) -> (Driver, String) {
    let run_id = format!("t{}x{}", std::process::id(), next_id());
    let prefix = format!("petri-{run_id}-");
    let executor: Arc<dyn Executor> =
        Arc::new(DockerExecutor::new(dir.path(), &run_id).with_retention(config.keep_workspaces));
    let driver = Driver::new(
        graph,
        executor,
        runners(),
        Arc::new(MapSecrets::empty()),
        config,
    );
    (driver, prefix)
}

pub fn docker_driver(graph: Graph, dir: &RunDir, config: RunConfig) -> Driver {
    docker_driver_named(graph, dir, config).0
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

/// Wait for a file to appear, so a test can act once a step is really running.
pub async fn wait_for_file(path: &Path, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

pub fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Every log line the run recorded, in order.
pub fn log_lines(report: &RunReport) -> Vec<String> {
    report
        .state
        .log
        .events()
        .filter_map(|e| match e {
            engine::Event::StepProgress {
                ev: ir::StepEvent::Log { line, .. },
                ..
            } => Some(line.clone()),
            _ => None,
        })
        .collect()
}

/// The status a node ended with.
pub fn status_of(report: &RunReport, name: &str) -> Option<String> {
    report
        .state
        .history()
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.outcome.status.tag().to_string())
}

pub fn output_of(report: &RunReport, name: &str) -> Value {
    report
        .state
        .history()
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.outcome.output.clone())
        .unwrap_or(Value::Null)
}

/// Names of the nodes that actually started, in order.
pub fn started(report: &RunReport) -> Vec<String> {
    report
        .state
        .log
        .records()
        .iter()
        .filter_map(|r| match &r.event {
            engine::Event::StepStarted { firing, .. } => Some(*firing),
            _ => None,
        })
        .filter_map(|firing| {
            report
                .state
                .history()
                .iter()
                .find(|h| h.firing == firing)
                .map(|h| h.name.to_string())
        })
        .collect()
}

/// Replay the run's log and assert it comes back byte-identical.
pub fn assert_replay_identical(graph: &Graph, report: &RunReport) {
    if let Err(mismatch) = engine::verify_replay(graph.clone(), &report.state.log) {
        panic!("replay was not byte-identical: {mismatch}");
    }
}

pub async fn docker_available() -> bool {
    DockerExecutor::is_available().await
}

/// Scope env as a plain map, for building scope specs in tests.
pub fn env(pairs: &[(&str, &str)]) -> BTreeMap<smol_str::SmolStr, ir::ExprOrValue> {
    pairs
        .iter()
        .map(|(k, v)| (smol_str::SmolStr::new(*k), ir::ExprOrValue::Value(json!(v))))
        .collect()
}

pub const RETAIN: Retention = Retention::Always;
