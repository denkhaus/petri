//! Fork admission across child invocations and the run-wide invocation
//! ceiling.
//!
//! A parent that forks work into child invocations names one gate for the
//! fork and a slot count. Every child is declared at once, but a child's
//! driver starts only on a free slot, in declaration order, and keeps the
//! slot until the driver ends, so the fork never has more live children than
//! slots. A child waiting out a retry backoff holds none, so a queued
//! sibling runs first. A child cancelled while it waits still starts, under
//! the bound, and finishes as cancelled; a resume queues the declared but
//! unfinished children again. The ceiling counts every invocation a run ever
//! declared, finished ones included, cannot be raised above 10,000 or
//! disabled, and survives a resume.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use execution::{
    AttemptAdmission, CallSite, Coordinator, CoordinatorError, CoordinatorEvent,
    CoordinatorInvocationClient, CoordinatorOptions, CoordinatorRecord, ExecutionId,
    ExecutionObserver, GraphDigest, InvocationClient as _, InvocationId, InvocationLimitError,
    InvocationRequest, InvokeError, MAX_INVOCATIONS, SandboxMode, SecretBindings,
};
use ir::{
    Backoff, FailureClass, FailureInfo, GraphBuilder, Outcome, ResultProjection, RetryOn,
    RetryPolicy, RunStatus, ScopeId, Status, StepRef, Value,
};
use runtime::engine::{EngineState, EventRecord};
use runtime::steps::{Step, StepCtx};
use runtime::{RunOptions, Runtime};
use serde::Deserialize;
use serde_json::json;
use testkit::RunDir;
use tokio::sync::Notify;
use tokio::time::{sleep, timeout};

/// What the traced steps saw: `(label, when)` in the order they happened.
#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<(String, Instant)>>>);

impl Trace {
    fn record(&self, label: impl Into<String>) {
        self.0
            .lock()
            .expect("not poisoned")
            .push((label.into(), Instant::now()));
    }

    fn labels(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("not poisoned")
            .iter()
            .map(|(label, _)| label.clone())
            .collect()
    }

    fn at(&self, label: &str) -> Instant {
        self.0
            .lock()
            .expect("not poisoned")
            .iter()
            .find(|(l, _)| l == label)
            .map_or_else(
                || panic!("`{label}` in {:?}", self.labels()),
                |(_, when)| *when,
            )
    }
}

/// Signalled by the first attempt of the traced step named `a`.
#[derive(Clone, Default)]
struct AStarted(Arc<Notify>);

#[derive(Deserialize)]
struct TraceConfig {
    name:       String,
    #[serde(default)]
    fail_first: bool,
    #[serde(default)]
    hold_ms:    u64,
}

/// Records `<name>:<attempt>` at its start and `<name>:<attempt>:end` at its
/// end; fails its first attempt with class `flaky` when asked.
struct TraceStep;

#[async_trait::async_trait]
impl Step for TraceStep {
    const NAME: &'static str = "test/trace";
    type Config = TraceConfig;

    async fn run(&self, config: TraceConfig, ctx: StepCtx) -> Outcome {
        let trace = ctx.capability::<Trace>().expect("trace");
        let attempt = ctx.attempt.raw();
        trace.record(format!("{}:{attempt}", config.name));
        if config.name == "a"
            && attempt == 1
            && let Some(started) = ctx.capability::<AStarted>()
        {
            started.0.notify_one();
        }
        if config.hold_ms > 0 {
            sleep(Duration::from_millis(config.hold_ms)).await;
        }
        trace.record(format!("{}:{attempt}:end", config.name));
        if config.fail_first && attempt == 1 {
            return Outcome::new(
                Status::Failure(FailureInfo::new("flaky").with_class(FailureClass::new("flaky"))),
                Value::Null,
            );
        }
        Outcome::success(json!({ "name": config.name, "attempt": attempt }))
    }
}

/// Waits for the `a` child's first attempt before it completes.
struct AfterAStep;

#[async_trait::async_trait]
impl Step for AfterAStep {
    const NAME: &'static str = "test/after-a";
    type Config = ();

    async fn run(&self, (): (), ctx: StepCtx) -> Outcome {
        let started = ctx.capability::<AStarted>().expect("a-started");
        started.0.notified().await;
        Outcome::success(Value::Null)
    }
}

#[derive(Deserialize)]
struct HoldConfig {
    name:   String,
    #[serde(default)]
    marker: Option<PathBuf>,
}

/// Holds its slot until its marker file is gone (forever without one) or
/// until it is stopped, and then finishes `Cancelled`. Records
/// `hold:<name>` at its start and `hold:<name>:end` at its end, and
/// signals [`AStarted`] when it starts.
struct HoldStep;

#[async_trait::async_trait]
impl Step for HoldStep {
    const NAME: &'static str = "test/hold";
    type Config = HoldConfig;

    async fn run(&self, config: HoldConfig, mut ctx: StepCtx) -> Outcome {
        let trace = ctx.capability::<Trace>().expect("trace");
        trace.record(format!("hold:{}", config.name));
        if let Some(started) = ctx.capability::<AStarted>() {
            started.0.notify_one();
        }
        let released = async {
            loop {
                if config
                    .marker
                    .as_ref()
                    .is_some_and(|marker| !marker.exists())
                {
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
        };
        let outcome = tokio::select! {
            () = released => Outcome::success(json!({ "name": config.name })),
            _ = ctx.control.recv() => Outcome::cancelled(),
        };
        trace.record(format!("hold:{}:end", config.name));
        outcome
    }
}

/// The child drivers live at once, seen from the coordinator records: a
/// child's `ExecutionDeclared` opens one and its `ExecutionFinished` closes
/// it. Also the order the children were declared and dispatched in.
#[derive(Default)]
struct LiveChildren(Mutex<LiveChildrenSeen>);

#[derive(Clone, Default)]
struct LiveChildrenSeen {
    executions: BTreeMap<ExecutionId, InvocationId>,
    live:       usize,
    peak:       usize,
    declared:   Vec<InvocationId>,
    dispatched: Vec<InvocationId>,
}

impl LiveChildren {
    fn seen(&self) -> LiveChildrenSeen {
        self.0.lock().expect("not poisoned").clone()
    }
}

impl ExecutionObserver for LiveChildren {
    fn on_engine_record(
        &self,
        _: ExecutionId,
        _: &EventRecord,
        _recorded_at: u64,
        _: &EngineState,
    ) {
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        let mut seen = self.0.lock().expect("not poisoned");
        match &record.body {
            CoordinatorEvent::InvocationDeclared {
                invocation,
                call: Some(_),
                ..
            } => seen.declared.push(*invocation),
            CoordinatorEvent::ExecutionDeclared {
                execution,
                invocation,
                ..
            } if *invocation != InvocationId::ROOT => {
                seen.dispatched.push(*invocation);
                seen.executions.insert(*execution, *invocation);
                seen.live += 1;
                seen.peak = seen.peak.max(seen.live);
            }
            CoordinatorEvent::ExecutionFinished { execution, .. }
                if seen.executions.contains_key(execution) =>
            {
                seen.live -= 1;
            }
            _ => {}
        }
    }
}

/// The most `start` labels open at once in `labels`, where `<start>:end`
/// closes one.
fn peak_concurrency(labels: &[String], start: &str) -> usize {
    let end = format!("{start}:end");
    let (mut open, mut peak) = (0usize, 0usize);
    for label in labels {
        if *label == start {
            open += 1;
            peak = peak.max(open);
        } else if *label == end {
            open -= 1;
        }
    }
    peak
}

fn slot_of(coordinator: &Coordinator, invocation: InvocationId) -> String {
    coordinator.store().state().invocations[&invocation]
        .declaration
        .call
        .as_ref()
        .expect("a child has a call")
        .slot
        .to_string()
}

#[derive(Deserialize)]
struct InvokeConfig {
    graph:        GraphDigest,
    slot:         String,
    #[serde(default)]
    gate:         Option<String>,
    #[serde(default)]
    max_parallel: u32,
}

/// Starts a child that inherits the caller's sandbox, under the fork gate
/// when one is named, and returns the child's status. A refused declaration
/// is this node's failure, with the coordinator's message.
struct InvokeStep;

#[async_trait::async_trait]
impl Step for InvokeStep {
    const NAME: &'static str = "test/invoke";
    type Config = InvokeConfig;

    async fn run(&self, config: InvokeConfig, ctx: StepCtx) -> Outcome {
        let client = match ctx.require_capability::<CoordinatorInvocationClient>() {
            Ok(client) => client,
            Err(error) => return error.into(),
        };
        let request = InvocationRequest {
            site:      CallSite {
                firing:  ctx.firing,
                attempt: ctx.attempt,
                slot:    config.slot.as_str().into(),
            },
            graph:     config.graph,
            context:   BTreeMap::new(),
            secrets:   SecretBindings::None,
            sandbox:   SandboxMode::Inherit { scope: ctx.scope },
            admission: config.gate.map(|gate| AttemptAdmission {
                gate:         gate.into(),
                max_parallel: config.max_parallel,
            }),
        };
        let mut handle = match client.start_or_attach(request).await {
            Ok(handle) => handle,
            Err(error @ InvokeError::InvocationLimit { .. }) => {
                if let Some(trace) = ctx.capability::<Trace>() {
                    trace.record(format!("refused:{}", config.slot));
                }
                return Outcome::new(
                    Status::Failure(
                        FailureInfo::new(error.to_string())
                            .with_class(FailureClass::new("invocation_limit")),
                    ),
                    Value::Null,
                );
            }
            Err(error) => return Outcome::failure(error.to_string()),
        };
        let result = handle.result().await;
        match result.status {
            RunStatus::Success => Outcome::success(result.output),
            RunStatus::Failed => Outcome::new(
                Status::Failure(
                    result
                        .failure
                        .unwrap_or_else(|| FailureInfo::new("child failed")),
                ),
                result.output,
            ),
            RunStatus::Cancelled => Outcome::cancelled(),
        }
    }
}

fn runtime(directory: &RunDir, trace: &Trace, started: &AStarted) -> Runtime {
    Runtime::standard()
        .step(TraceStep)
        .step(AfterAStep)
        .step(HoldStep)
        .step(InvokeStep)
        .capability(trace.clone())
        .capability(started.clone())
        .options(RunOptions::new(directory.path()))
}

/// A child graph of one traced node, with a retry policy that retries the
/// `flaky` class once after a short fixed backoff.
fn traced_child(name: &str, fail_first: bool, hold_ms: u64) -> ir::Graph {
    let mut child = GraphBuilder::new();
    let node = child.add_node(
        name,
        ScopeId::new(0),
        StepRef::new(
            TraceStep::NAME,
            json!({ "name": name, "fail_first": fail_first, "hold_ms": hold_ms }),
        ),
    );
    child.node_mut(node).retry = RetryPolicy::attempts(2)
        .with_backoff(Backoff {
            initial: Duration::from_millis(400),
            factor:  1.0,
            max:     Duration::from_secs(1),
            jitter:  false,
        })
        .with_retry_on(RetryOn::classes(&[FailureClass::new("flaky")]));
    child.build()
}

/// A child graph of one holding node named `h`.
fn hold_child(marker: Option<&Path>) -> ir::Graph {
    let mut child = GraphBuilder::new();
    child.add_node(
        "h",
        ScopeId::new(0),
        StepRef::new(HoldStep::NAME, json!({ "name": "h", "marker": marker })),
    );
    child.build()
}

/// A parent of `count` branches `b00..`, each starting `child` under
/// `fork@1` with `max_parallel` slots, at call slots `c00..`.
fn fork_parent(child: GraphDigest, count: usize, max_parallel: u32) -> ir::Graph {
    let mut parent = GraphBuilder::new();
    for index in 0..count {
        parent.add_node(
            format!("b{index:02}").as_str(),
            ScopeId::new(0),
            invoke(child, &format!("c{index:02}"), Some("fork@1"), max_parallel),
        );
    }
    parent.build()
}

fn invoke(graph: GraphDigest, slot: &str, gate: Option<&str>, max_parallel: u32) -> StepRef {
    StepRef::new(
        InvokeStep::NAME,
        json!({ "graph": graph, "slot": slot, "gate": gate, "max_parallel": max_parallel }),
    )
}

/// `max_parallel = 1`: branch A fails its first attempt and backs off; B,
/// queued behind the one slot, runs during A's backoff; A's second attempt
/// follows.
#[tokio::test]
async fn a_backoff_releases_the_fork_slot_so_a_queued_branch_runs_first() {
    let directory = RunDir::new("admission-backoff");
    let trace = Trace::default();
    let started = AStarted::default();
    let runtime = runtime(&directory, &trace, &started);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .expect("the coordinator starts");
    let child_a = coordinator
        .register_graph(&traced_child("a", true, 0))
        .expect("a registers");
    let child_b = coordinator
        .register_graph(&traced_child("b", false, 0))
        .expect("b registers");
    let mut parent = GraphBuilder::new();
    parent.add_node(
        "branch_a",
        ScopeId::new(0),
        invoke(child_a, "a", Some("fork@1"), 1),
    );
    let after_a = parent.add_node(
        "after_a",
        ScopeId::new(0),
        StepRef::new(AfterAStep::NAME, Value::Null),
    );
    let branch_b = parent.add_node(
        "branch_b",
        ScopeId::new(0),
        invoke(child_b, "b", Some("fork@1"), 1),
    );
    parent.link(after_a, branch_b);
    let parent = coordinator
        .register_graph(&parent.build())
        .expect("parent registers");
    let result = timeout(
        Duration::from_secs(20),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the fork completes")
    .expect("the run completes");
    assert_eq!(result.status, RunStatus::Success);
    let labels = trace.labels();
    let position = |label: &str| {
        labels
            .iter()
            .position(|l| l == label)
            .unwrap_or_else(|| panic!("`{label}` in {labels:?}"))
    };
    assert!(
        position("a:1:end") < position("b:1") && position("b:1:end") < position("a:2"),
        "B runs in A's backoff, before A's second attempt: {labels:?}"
    );
    assert!(
        trace.at("a:2") >= trace.at("a:1:end") + Duration::from_millis(300),
        "A backed off before its second attempt"
    );
    assert_eq!(coordinator.store().state().invocations.len(), 3);
    coordinator.finish().await;
}

/// Two children under one gate: with one slot their attempts never overlap;
/// with two they do.
async fn overlap_under(max_parallel: u32) -> bool {
    let directory = RunDir::new("admission-overlap");
    let trace = Trace::default();
    let started = AStarted::default();
    let runtime = runtime(&directory, &trace, &started);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .expect("the coordinator starts");
    let child_a = coordinator
        .register_graph(&traced_child("a", false, 300))
        .expect("a registers");
    let child_b = coordinator
        .register_graph(&traced_child("b", false, 300))
        .expect("b registers");
    let mut parent = GraphBuilder::new();
    parent.add_node(
        "branch_a",
        ScopeId::new(0),
        invoke(child_a, "a", Some("fork@1"), max_parallel),
    );
    parent.add_node(
        "branch_b",
        ScopeId::new(0),
        invoke(child_b, "b", Some("fork@1"), max_parallel),
    );
    let parent = coordinator
        .register_graph(&parent.build())
        .expect("parent registers");
    let result = timeout(
        Duration::from_secs(20),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the fork completes")
    .expect("the run completes");
    assert_eq!(result.status, RunStatus::Success);
    coordinator.finish().await;
    let (first, second) = if trace.at("a:1") <= trace.at("b:1") {
        ("a", "b")
    } else {
        ("b", "a")
    };
    trace.at(&format!("{second}:1")) < trace.at(&format!("{first}:1:end"))
}

#[tokio::test]
async fn one_slot_serializes_the_attempts_of_a_fork_and_two_slots_let_them_overlap() {
    assert!(
        !overlap_under(1).await,
        "with one slot the second attempt starts after the first ends"
    );
    assert!(
        overlap_under(2).await,
        "with two slots the attempts overlap"
    );
}

#[test]
fn a_limit_of_zero_or_above_the_ceiling_is_refused() {
    assert_eq!(MAX_INVOCATIONS, 10_000);
    assert_eq!(
        CoordinatorOptions::default().with_max_invocations(0).err(),
        Some(InvocationLimitError::Disabled)
    );
    assert_eq!(
        CoordinatorOptions::default()
            .with_max_invocations(MAX_INVOCATIONS + 1)
            .err(),
        Some(InvocationLimitError::AboveCeiling {
            requested: 10_001,
            ceiling:   10_000,
        })
    );
    assert_eq!(
        CoordinatorOptions::default()
            .with_max_invocations(MAX_INVOCATIONS)
            .map(|options| options.max_invocations),
        Ok(10_000)
    );
    let directory = RunDir::new("admission-refused-options");
    let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
    let options = CoordinatorOptions {
        max_invocations: 0,
        ..CoordinatorOptions::default()
    };
    assert!(matches!(
        Coordinator::create(runtime.prepare_run(directory.path()), Vec::new(), options),
        Err(CoordinatorError::InvalidInvocationLimit(
            InvocationLimitError::Disabled
        ))
    ));
    let options = CoordinatorOptions {
        max_invocations: MAX_INVOCATIONS + 1,
        ..CoordinatorOptions::default()
    };
    assert!(matches!(
        Coordinator::create(runtime.prepare_run(directory.path()), Vec::new(), options),
        Err(CoordinatorError::InvalidInvocationLimit(
            InvocationLimitError::AboveCeiling { .. }
        ))
    ));
}

fn noop_child() -> ir::Graph {
    let mut child = GraphBuilder::new();
    child.add_step("only", ScopeId::new(0), "noop");
    child.build()
}

/// Three sequential children under a limit of 3 (the root counts): the
/// third is refused before its step runs, even though the first two
/// finished, and the refusal names the total, the limit, the parent
/// execution, the firing and the call slot.
#[tokio::test]
async fn the_limit_counts_finished_children_and_names_the_refused_call() {
    let directory = RunDir::new("admission-limit-sequence");
    let trace = Trace::default();
    let started = AStarted::default();
    let runtime = runtime(&directory, &trace, &started);
    let options = CoordinatorOptions::default()
        .with_max_invocations(3)
        .expect("a lower limit is allowed");
    let mut coordinator =
        Coordinator::create(runtime.prepare_run(directory.path()), Vec::new(), options)
            .expect("the coordinator starts");
    let child = coordinator
        .register_graph(&traced_child("c", false, 0))
        .expect("child registers");
    let mut parent = GraphBuilder::new();
    let first = parent.add_node("first", ScopeId::new(0), invoke(child, "first", None, 0));
    let second = parent.add_node("second", ScopeId::new(0), invoke(child, "second", None, 0));
    let third = parent.add_node("third", ScopeId::new(0), invoke(child, "third", None, 0));
    parent.link(first, second);
    parent.link(second, third);
    let parent = coordinator
        .register_graph(&parent.build())
        .expect("parent registers");
    let result = timeout(
        Duration::from_secs(20),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the run completes")
    .expect("the run completes");
    assert_eq!(result.status, RunStatus::Failed);
    let failure = result.failure.expect("the refused call failed the run");
    assert_eq!(failure.class.as_str(), "invocation_limit");
    assert!(
        failure.message.contains("3 of 3 invocations are declared"),
        "{}",
        failure.message
    );
    assert!(
        failure.message.contains("execution 0") && failure.message.contains("`third`"),
        "the refusal names the parent execution and the call slot: {}",
        failure.message
    );
    assert_eq!(coordinator.store().state().invocations.len(), 3);
    assert_eq!(
        trace
            .labels()
            .iter()
            .filter(|label| label.starts_with("c:"))
            .count(),
        4,
        "two children ran (start and end each); the third never started: {:?}",
        trace.labels()
    );
    assert!(trace.labels().contains(&"refused:third".to_owned()));
    coordinator.finish().await;
}

/// A child's own children count against the run's limit.
#[tokio::test]
async fn nested_invocations_count_against_the_same_limit() {
    let directory = RunDir::new("admission-limit-nested");
    let trace = Trace::default();
    let started = AStarted::default();
    let runtime = runtime(&directory, &trace, &started);
    let options = CoordinatorOptions::default()
        .with_max_invocations(3)
        .expect("a lower limit is allowed");
    let mut coordinator =
        Coordinator::create(runtime.prepare_run(directory.path()), Vec::new(), options)
            .expect("the coordinator starts");
    let grandchild = coordinator
        .register_graph(&noop_child())
        .expect("grandchild registers");
    let mut child = GraphBuilder::new();
    let one = child.add_node("one", ScopeId::new(0), invoke(grandchild, "one", None, 0));
    let two = child.add_node("two", ScopeId::new(0), invoke(grandchild, "two", None, 0));
    child.link(one, two);
    let child = coordinator
        .register_graph(&child.build())
        .expect("child registers");
    let mut parent = GraphBuilder::new();
    parent.add_node("child", ScopeId::new(0), invoke(child, "child", None, 0));
    let parent = coordinator
        .register_graph(&parent.build())
        .expect("parent registers");
    let result = timeout(
        Duration::from_secs(20),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the run completes")
    .expect("the run completes");
    assert_eq!(result.status, RunStatus::Failed);
    // Root, child, one grandchild: the second grandchild is refused.
    assert_eq!(coordinator.store().state().invocations.len(), 3);
    assert!(trace.labels().contains(&"refused:two".to_owned()));
    coordinator.finish().await;
}

/// A finished run at its limit resumes under the same limit (a replay
/// declares nothing) and refuses a lower one that its declarations already
/// exceed.
#[tokio::test]
async fn resume_keeps_the_limit_and_refuses_one_below_the_declared_total() {
    let directory = RunDir::new("admission-limit-resume");
    let trace = Trace::default();
    let started = AStarted::default();
    let runtime = runtime(&directory, &trace, &started);
    let options = CoordinatorOptions::default()
        .with_max_invocations(3)
        .expect("a lower limit is allowed");
    let mut coordinator =
        Coordinator::create(runtime.prepare_run(directory.path()), Vec::new(), options)
            .expect("the coordinator starts");
    let child = coordinator
        .register_graph(&noop_child())
        .expect("child registers");
    let mut parent = GraphBuilder::new();
    let first = parent.add_node("first", ScopeId::new(0), invoke(child, "first", None, 0));
    let second = parent.add_node("second", ScopeId::new(0), invoke(child, "second", None, 0));
    parent.link(first, second);
    let parent = coordinator
        .register_graph(&parent.build())
        .expect("parent registers");
    let result = coordinator
        .run_root(parent, BTreeMap::new())
        .await
        .expect("the run completes");
    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(coordinator.store().state().invocations.len(), 3);
    coordinator.finish().await;

    // The same limit: the replay reattaches to the declared children and
    // declares nothing new.
    let (mut resumed, _) =
        Coordinator::resume(runtime.prepare_run(directory.path()), Vec::new(), options)
            .await
            .expect("resumes at the boundary");
    let result = resumed
        .run_root(parent, BTreeMap::new())
        .await
        .expect("the replay completes");
    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(resumed.store().state().invocations.len(), 3);
    resumed.finish().await;

    // A lower limit than the run already holds is refused at resume.
    let lower = CoordinatorOptions::default()
        .with_max_invocations(2)
        .expect("a lower limit is allowed");
    match Coordinator::resume(runtime.prepare_run(directory.path()), Vec::new(), lower).await {
        Err(CoordinatorError::InvocationLimit { total, limit }) => {
            assert_eq!((total, limit), (3, 2));
        }
        Ok(_) => panic!("a resume below the declared total is refused"),
        Err(other) => panic!("unexpected error: {other}"),
    }
}

/// A step that starts children one after another until one is refused, and
/// reports how many were admitted.
#[derive(Deserialize)]
struct FloodConfig {
    graph: GraphDigest,
    count: u32,
}

struct FloodStep;

#[async_trait::async_trait]
impl Step for FloodStep {
    const NAME: &'static str = "test/flood";
    type Config = FloodConfig;

    async fn run(&self, config: FloodConfig, ctx: StepCtx) -> Outcome {
        let client = match ctx.require_capability::<CoordinatorInvocationClient>() {
            Ok(client) => client,
            Err(error) => return error.into(),
        };
        let mut admitted = 0_u32;
        let mut refused = None;
        for index in 0..config.count {
            let request = InvocationRequest::new(
                CallSite {
                    firing:  ctx.firing,
                    attempt: ctx.attempt,
                    slot:    format!("child-{index}").into(),
                },
                config.graph,
                BTreeMap::new(),
                SecretBindings::None,
                SandboxMode::Inherit { scope: ctx.scope },
            );
            match client.start_or_attach(request).await {
                Ok(mut handle) => {
                    admitted += 1;
                    let _ = handle.result().await;
                }
                Err(error) => {
                    refused = Some(error.to_string());
                    break;
                }
            }
        }
        Outcome::success(json!({ "admitted": admitted, "refused": refused }))
    }
}

/// The literal ceiling: 9,999 children after the root are admitted and the
/// 10,001st invocation is refused before its step runs. Every child is a
/// durable invocation with its own execution log and five `fsync`s, so this
/// takes about five minutes; it runs in the extended gate.
#[tokio::test]
#[ignore = "declares 10,000 durable invocations; run in the extended gate"]
async fn exactly_ten_thousand_invocations_are_admitted_and_the_next_is_refused() {
    let directory = RunDir::new("admission-ten-thousand");
    let runtime = Runtime::standard()
        .step(FloodStep)
        .options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default()
            .with_max_invocations(MAX_INVOCATIONS)
            .expect("the ceiling itself"),
    )
    .expect("the coordinator starts");
    let child = coordinator
        .register_graph(&noop_child())
        .expect("child registers");
    let mut parent = GraphBuilder::new();
    let flood = parent.add_node(
        "flood",
        ScopeId::new(0),
        StepRef::new(
            FloodStep::NAME,
            json!({ "graph": child, "count": MAX_INVOCATIONS }),
        ),
    );
    parent.graph_mut().result = ResultProjection::NodeOutput(flood);
    let parent = coordinator
        .register_graph(&parent.build())
        .expect("parent registers");
    let result = coordinator
        .run_root(parent, BTreeMap::new())
        .await
        .expect("the run completes");
    assert_eq!(result.status, RunStatus::Success);
    let flood = result.output;
    assert_eq!(flood["admitted"], json!(MAX_INVOCATIONS - 1));
    assert!(
        flood["refused"]
            .as_str()
            .is_some_and(|m| m.contains("10000 of 10000 invocations are declared")),
        "{flood}"
    );
    assert_eq!(
        coordinator.store().state().invocations.len(),
        MAX_INVOCATIONS as usize
    );
    coordinator.finish().await;
}

/// Fifty children under four slots: at most four child drivers are live at
/// once, and they start in declaration order, which is branch order.
#[tokio::test]
async fn a_fork_of_fifty_children_keeps_four_live_and_starts_them_in_order() {
    let directory = RunDir::new("admission-live-bound");
    let trace = Trace::default();
    let started = AStarted::default();
    let runtime = runtime(&directory, &trace, &started);
    let live = Arc::new(LiveChildren::default());
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .expect("the coordinator starts")
    .observe(live.clone());
    let child = coordinator
        .register_graph(&traced_child("c", false, 20))
        .expect("child registers");
    let parent = coordinator
        .register_graph(&fork_parent(child, 50, 4))
        .expect("parent registers");
    let result = timeout(
        Duration::from_secs(60),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the fork completes")
    .expect("the run completes");
    assert_eq!(result.status, RunStatus::Success);
    let seen = live.seen();
    assert_eq!(seen.peak, 4, "four child drivers are live at the most");
    assert_eq!(seen.live, 0, "every child driver finished");
    assert_eq!(seen.dispatched.len(), 50);
    assert_eq!(
        seen.dispatched, seen.declared,
        "children start in declaration order"
    );
    let slots: Vec<String> = seen
        .declared
        .iter()
        .map(|invocation| slot_of(&coordinator, *invocation))
        .collect();
    let expected: Vec<String> = (0..50).map(|index| format!("c{index:02}")).collect();
    assert_eq!(slots, expected, "declaration order is branch order");
    assert!(
        peak_concurrency(&trace.labels(), "c:1") <= 4,
        "at most four child steps run at once: {:?}",
        trace.labels()
    );
    coordinator.finish().await;
}

/// Twelve holding children under one slot, cancelled while eleven wait for
/// a slot: every child finishes as cancelled, each after its own driver ran
/// once under the bound, and none is left live or queued.
#[tokio::test]
async fn cancelling_the_parent_finishes_every_queued_child_as_cancelled() {
    let directory = RunDir::new("admission-cancel-queued");
    let trace = Trace::default();
    let started = AStarted::default();
    let runtime = runtime(&directory, &trace, &started);
    let live = Arc::new(LiveChildren::default());
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .expect("the coordinator starts")
    .observe(live.clone());
    let child = coordinator
        .register_graph(&hold_child(None))
        .expect("child registers");
    let parent = coordinator
        .register_graph(&fork_parent(child, 12, 1))
        .expect("parent registers");
    let handle = coordinator.handle();
    let first_started = started.clone();
    tokio::spawn(async move {
        first_started.0.notified().await;
        handle.cancel_root();
    });
    let result = timeout(
        Duration::from_secs(30),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the cancelled fork settles")
    .expect("the run completes");
    assert_eq!(result.status, RunStatus::Cancelled);
    let state = coordinator.store().state();
    assert_eq!(state.invocations.len(), 13);
    for index in 1..=12 {
        let invocation = &state.invocations[&InvocationId::new(index)];
        assert!(invocation.cancelled, "child {index} was cancelled");
        assert_eq!(
            invocation.result.as_ref().map(|result| result.status),
            Some(RunStatus::Cancelled),
            "child {index} finished as cancelled"
        );
        assert_eq!(
            invocation.executions.len(),
            1,
            "child {index} ran one execution"
        );
    }
    let seen = live.seen();
    assert_eq!(
        seen.dispatched.len(),
        12,
        "every queued child got its driver"
    );
    assert_eq!(seen.peak, 1, "one child driver is live at the most");
    assert_eq!(seen.live, 0, "no child driver is left live");
    assert_eq!(
        trace
            .labels()
            .iter()
            .filter(|label| *label == "hold:h")
            .count(),
        1,
        "only the admitted child ran its step: {:?}",
        trace.labels()
    );
    coordinator.finish().await;
}

/// Six holding children under two slots. The run crashes while two are live
/// and four wait for a slot. The resumed run resumes the two without
/// declaring them again and dispatches the four under the same bound, in
/// declaration order.
#[tokio::test]
async fn resume_redispatches_queued_children_under_the_bound() {
    let directory = RunDir::new("admission-resume-queued");
    let marker = directory.path().join("hold");
    fs::write(&marker, b"").expect("the marker is written");
    let trace = Trace::default();
    let started = AStarted::default();
    let first_runtime = runtime(&directory, &trace, &started);
    let live = Arc::new(LiveChildren::default());
    let mut coordinator = Coordinator::create(
        first_runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .expect("the coordinator starts")
    .observe(live.clone());
    let child = coordinator
        .register_graph(&hold_child(Some(&marker)))
        .expect("child registers");
    let parent = coordinator
        .register_graph(&fork_parent(child, 6, 2))
        .expect("parent registers");
    {
        let run = coordinator.run_root(parent, BTreeMap::new());
        tokio::pin!(run);
        let two_live = async {
            loop {
                let seen = live.seen();
                let holding = trace
                    .labels()
                    .iter()
                    .filter(|label| *label == "hold:h")
                    .count();
                if seen.declared.len() == 6 && seen.dispatched.len() == 2 && holding == 2 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            result = &mut run => panic!("the run finished before the crash: {result:?}"),
            waited = timeout(Duration::from_secs(20), two_live) => {
                waited.expect("two children are live and four wait");
            }
        }
        // The crash: the run future drops here, and its drivers with it.
    }
    drop(coordinator);
    let before = live.seen();
    let labels_before = trace.labels().len();
    fs::remove_file(&marker).expect("the marker is removed");

    let resumed_runtime = runtime(&directory, &trace, &started);
    let resumed_live = Arc::new(LiveChildren::default());
    let (coordinator, _) = Coordinator::resume(
        resumed_runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator resumes");
    let mut coordinator = coordinator.observe(resumed_live.clone());
    let result = timeout(
        Duration::from_secs(30),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the resumed fork completes")
    .expect("the resumed run completes");
    assert_eq!(result.status, RunStatus::Success);
    let state = coordinator.store().state();
    assert_eq!(state.invocations.len(), 7, "no child was declared again");
    for index in 1..=6 {
        let invocation = &state.invocations[&InvocationId::new(index)];
        assert_eq!(
            invocation.executions.len(),
            1,
            "child {index} has one execution"
        );
        assert_eq!(
            invocation.result.as_ref().map(|result| result.status),
            Some(RunStatus::Success),
            "child {index} succeeded"
        );
    }
    let after = resumed_live.seen();
    let queued: Vec<InvocationId> = before
        .declared
        .iter()
        .copied()
        .filter(|invocation| !before.dispatched.contains(invocation))
        .collect();
    assert_eq!(queued.len(), 4);
    assert_eq!(
        after.dispatched, queued,
        "the queued children get their first execution on resume, in order"
    );
    assert!(
        after.peak <= 2,
        "at most two new drivers live: {}",
        after.peak
    );
    assert_eq!(after.live, 0);
    assert!(
        peak_concurrency(&trace.labels()[labels_before..], "hold:h") <= 2,
        "at most two child steps run at once after the resume: {:?}",
        &trace.labels()[labels_before..]
    );
    coordinator.finish().await;
}
