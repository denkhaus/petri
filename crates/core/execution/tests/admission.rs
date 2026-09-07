//! Attempt admission across child invocations and the run-wide invocation
//! ceiling.
//!
//! A parent that forks work into child invocations names one gate for the
//! fork and a slot count; every attempt of every child under that gate takes
//! a slot before it runs and releases it when the attempt ends, so a child
//! waiting out a retry backoff holds none and a queued sibling runs first.
//! The ceiling counts every invocation a run ever declared, finished ones
//! included, cannot be raised above 10,000 or disabled, and survives a
//! resume.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use execution::{
    AttemptAdmission, CallSite, Coordinator, CoordinatorError, CoordinatorInvocationClient,
    CoordinatorOptions, GraphDigest, InvocationClient as _, InvocationLimitError,
    InvocationRequest, InvokeError, MAX_INVOCATIONS, SandboxMode, SecretBindings,
};
use ir::{
    Backoff, FailureClass, FailureInfo, GraphBuilder, Outcome, ResultProjection, RetryOn,
    RetryPolicy, RunStatus, ScopeId, Status, StepRef, Value,
};
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
    match Coordinator::resume(runtime.prepare_run(directory.path()), Vec::new(), lower) {
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
/// durable invocation with its own execution log, so this takes about seven
/// minutes; it runs in the extended gate.
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
