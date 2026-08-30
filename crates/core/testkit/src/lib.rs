//! Shared test scaffolding.
//!
//! What every end-to-end harness needs and none should copy: a run directory
//! that cleans itself up, readers over a [`RunReport`], the replay canary as an
//! assertion, a step that ignores cancellation, and a step that needs a
//! capability. Dev-dependency only; never published.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{env, fs, process};

use driver::RunReport;
use executor::Retention;
use executor_docker::DockerExecutor;
use ir::{Graph, GraphBuilder, NodeId, ScopeId, StepRef, Value};
use serde::Deserialize;
use serde_json::json;
use steps::PROCESS_KIND;
use tokio::time;

/// A process-unique counter, for run ids and directory names.
pub fn unique_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// A run directory that cleans itself up.
pub struct RunDir {
    path: PathBuf,
}

impl RunDir {
    pub fn new(label: &str) -> Self {
        let unique = format!("{label}-{}-{}", process::id(), unique_id());
        let path = env::temp_dir().join("petri-tests").join(unique);
        fs::create_dir_all(&path).unwrap_or_else(|e| {
            panic!(
                "could not create the test run dir `{}`: {e}",
                path.display()
            )
        });
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
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A `run:` script step config.
pub fn script(run: &str) -> Value {
    json!({ "run": run })
}

pub fn script_with(run: &str, extra: &Value) -> Value {
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

/// A step kind that ignores `Control::Cancel` and never returns, so the
/// driver's hard deadline is the only thing that can end it.
pub struct WedgedStep;

pub const WEDGED_KIND: ir::StepKindId = ir::StepKindId::new_static("wedged");

impl ir::StepKind for WedgedStep {
    fn id(&self) -> ir::StepKindId {
        WEDGED_KIND
    }

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "wedged"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for WedgedStep {
    async fn run(&self, mut ctx: steps::StepCtx) -> ir::Outcome {
        ctx.log(ir::LogStream::Stdout, "wedged step is running")
            .await;
        // Receive the cancel and deliberately do nothing about it.
        let _ = ctx.control.recv().await;
        loop {
            time::sleep(Duration::from_hours(1)).await;
        }
    }
}

/// A step kind that returns configured [`ir::SpliceRequest`]s, so batteries
/// drive uploads with no component. Config shape:
/// `{ "requests": [<SpliceRequest>...], "output": <value?> }` — `requests`
/// deserialized as-is, `output` returned as the outcome's output.
pub struct SpliceStep;

pub const SPLICE_KIND: ir::StepKindId = ir::StepKindId::new_static("splice");

/// The config a [`SpliceStep`] node takes, for building graphs in tests.
pub fn splice_config(requests: &[ir::SpliceRequest], output: &Value) -> Value {
    json!({
        "requests": requests,
        "output": output,
    })
}

/// Typed config for [`SpliceStep`].
#[doc(hidden)]
#[derive(Deserialize)]
pub struct SpliceConfig {
    #[serde(default)]
    requests: Vec<ir::SpliceRequest>,
    #[serde(default)]
    output:   Value,
}

#[async_trait::async_trait]
impl steps::Step for SpliceStep {
    const NAME: &'static str = "splice";

    type Config = SpliceConfig;

    async fn run(&self, config: Self::Config, _ctx: steps::StepCtx) -> ir::Outcome {
        ir::Outcome::success(config.output).with_splices(config.requests)
    }
}

/// The handle a host registers as a capability: a concrete type over whatever
/// it wraps. [`GreetStep`] requires it.
pub struct Greeting(pub &'static str);

/// A step kind that requires the [`Greeting`] capability and outputs its text —
/// or, with no host having registered one, fails routably with
/// `capability_unavailable`.
pub struct GreetStep;

pub const GREET_KIND: ir::StepKindId = ir::StepKindId::new_static("greet");

impl ir::StepKind for GreetStep {
    fn id(&self) -> ir::StepKindId {
        GREET_KIND
    }

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "greet"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for GreetStep {
    async fn run(&self, ctx: steps::StepCtx) -> ir::Outcome {
        match ctx.require_capability::<Greeting>() {
            Ok(greeting) => ir::Outcome::success(json!(greeting.0)),
            Err(failure) => failure.into(),
        }
    }
}

/// Wait for a file to appear, so a test can act once a step is really running.
pub async fn wait_for_file(path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        time::sleep(Duration::from_millis(20)).await;
    }
    false
}

pub fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| m.len())
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
        .map_or(Value::Null, |r| r.outcome.output.clone())
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

/// Exactly one terminal `StepFinished` per firing, however many cancels, kills,
/// timeouts, or step returns raced to produce one.
pub fn assert_one_terminal_per_firing(report: &RunReport) {
    let mut finishes: BTreeMap<u64, usize> = BTreeMap::new();
    for event in report.state.log.events() {
        if let engine::Event::StepFinished { firing, .. } = event {
            *finishes.entry(firing.raw()).or_default() += 1;
        }
    }
    assert!(
        finishes.values().all(|n| *n == 1),
        "one terminal event per firing: {finishes:?}"
    );
}

pub async fn docker_available() -> bool {
    DockerExecutor::is_available().await
}

/// The skip-or-require convention every Docker battery shares: skip loudly
/// without a daemon, unless `PETRI_REQUIRE_DOCKER` says a silent skip must be
/// a failure (CI cannot tell a skipped battery from a passing one).
#[expect(
    clippy::print_stderr,
    reason = "the skip notice has to reach the test runner's output; testkit has no logging sink"
)]
pub async fn docker_ready() -> bool {
    if docker_available().await {
        return true;
    }
    assert!(
        !env::var("PETRI_REQUIRE_DOCKER").is_ok_and(|v| !v.is_empty()),
        "PETRI_REQUIRE_DOCKER is set, but no Docker daemon is reachable"
    );
    eprintln!("skipping: no Docker daemon reachable");
    false
}

/// Scope env as a plain map, for building scope specs in tests.
pub fn env(pairs: &[(&str, &str)]) -> BTreeMap<smol_str::SmolStr, ir::ExprOrValue> {
    pairs
        .iter()
        .map(|(k, v)| (smol_str::SmolStr::new(*k), ir::ExprOrValue::Value(json!(v))))
        .collect()
}

pub const RETAIN: Retention = Retention::Always;
