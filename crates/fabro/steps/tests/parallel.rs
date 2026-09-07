//! Fabro parallel nodes end to end on the host executor: every branch a
//! child invocation with its own context in the shared workspace, the
//! fan-in's envelopes in branch order, `parallel.results` published, and the
//! branch lifecycle across failure, duplicates, empty and labelled
//! `for_each` lists, repeated forks, nested forks, cancellation and resume.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs, thread};

use execution::host::{self, HostRun};
use execution::inspect::inspect_run;
use fabro_steps::{
    AGENT_KIND, BranchStep, CommandStep, FanInStep, HUMAN_KIND, STAGE_KIND, StubStep, WAIT_KIND,
    WORKFLOW_KIND,
};
use frontend::{CompileInputs, Lowered, NoFiles};
use runtime::driver::ExecutionReport;
use runtime::executor::Retention;
use runtime::ir::{Graph, RunStatus};
use runtime::{RunOptions, Runtime};
use serde_json::{Value, json};
use testkit::{RunDir, output_of, status_of};

fn dot(body: &str) -> String {
    format!("digraph T {{\n  start [shape=Mdiamond]\n  exit [shape=Msquare]\n{body}\n}}")
}

#[expect(
    clippy::print_stderr,
    reason = "a graph that fails to lower explains itself in the test output"
)]
fn lower(text: &str) -> Lowered {
    let lowered = frontend_fabro::load("test.fabro", text, &NoFiles, &CompileInputs::new());
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    assert!(lowered.graph.is_some(), "lowers");
    lowered
}

/// Real commands and the structural steps; stubs for the stages a model or a
/// person would run.
fn runtime(dir: &Path) -> Runtime {
    runtime_retaining(dir, Retention::Never)
}

fn runtime_retaining(dir: &Path, retention: Retention) -> Runtime {
    let mut options = RunOptions::new(dir);
    options.grace = Duration::from_secs(2);
    options.retention = retention;
    options.echo = false;
    let runtime = Runtime::standard().step(CommandStep);
    let mut registry = runtime.registry().clone();
    for kind in [AGENT_KIND, HUMAN_KIND, WAIT_KIND, WORKFLOW_KIND, STAGE_KIND] {
        registry.register_runner(Arc::new(StubStep::new(kind)));
    }
    registry.register(BranchStep);
    registry.register(FanInStep);
    runtime.steps(registry).options(options)
}

async fn run(rt: &Runtime, lowered: Lowered) -> ExecutionReport {
    run_with(rt, lowered, |_| {}).await
}

async fn run_with(
    rt: &Runtime,
    lowered: Lowered,
    with_handle: impl FnOnce(execution::CoordinatorHandle),
) -> ExecutionReport {
    let graph: Graph = lowered.graph.expect("lowers");
    host::run_configured(
        rt,
        HostRun::new(graph).with_children(lowered.children),
        |handle, _| with_handle(handle),
    )
    .await
    .expect("the run completes")
}

fn published_results(report: &ExecutionReport) -> Vec<Value> {
    report
        .state
        .run_context()
        .get("parallel.results")
        .and_then(Value::as_array)
        .cloned()
        .expect("parallel.results published")
}

fn invocation_count(dir: &Path) -> usize {
    inspect_run(dir).expect("inspects").invocations.len()
}

const FINDERS: &str = r#"
    fork [shape=component]
    a [shape=parallelogram, output_schema="routing", script="printf '%s' '{\"context_updates\":{\"output.finder\":{\"found\":\"a\"}}}'"]
    b [shape=parallelogram, output_schema="routing", script="printf '%s' '{\"context_updates\":{\"output.finder\":{\"found\":\"b\"}}}'"]
    merge [shape=tripleoctagon]
    report [shape=parallelogram, script="cat", stdin_source="context.parallel.results"]
    start -> fork
    fork -> a
    fork -> b
    a -> merge
    b -> merge
    merge -> report -> exit
"#;

#[tokio::test]
async fn static_branches_return_envelopes_that_never_merge_into_the_parent() {
    let dir = RunDir::new("parallel-static");
    let rt = runtime(dir.path());
    let report = run(&rt, lower(&dot(FINDERS))).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let results = published_results(&report);
    assert_eq!(results.len(), 2);
    for (index, (envelope, found)) in results.iter().zip(["a", "b"]).enumerate() {
        assert_eq!(envelope["id"], json!(found));
        assert_eq!(envelope["index"], json!(index));
        assert_eq!(envelope["status"], json!("succeeded"));
        assert!(
            envelope.get("item_label").is_none(),
            "static branches have no label"
        );
        assert_eq!(
            envelope["context_updates"]["output.finder"]["found"],
            json!(found)
        );
        assert!(envelope["context_updates"]["command.output"].is_string());
    }
    let kv = report.state.run_context();
    assert!(
        kv.get("output.finder").is_none(),
        "branch keys stay out of the parent"
    );
    assert_eq!(kv.get("parallel.branch_count"), Some(&json!(2)));
    assert_eq!(output_of(&report, "merge"), Value::Array(results.clone()));
    let stdin: Value = serde_json::from_str(
        output_of(&report, "report")["stdout"]
            .as_str()
            .expect("the report read stdin"),
    )
    .expect("stdin is the results JSON");
    assert_eq!(stdin, Value::Array(results));
    assert_eq!(
        invocation_count(dir.path()),
        3,
        "the root and one child per branch"
    );
}

#[tokio::test]
async fn mixed_failures_join_partially_and_all_failed_fails_the_fan_in() {
    let dir = RunDir::new("parallel-mixed");
    let rt = runtime(dir.path());
    let report = run(
        &rt,
        lower(&dot(r#"
        fork [shape=component]
        ok [shape=parallelogram, script="echo fine"]
        bad [shape=parallelogram, script="echo boom; exit 3"]
        merge [shape=tripleoctagon]
        start -> fork
        fork -> ok
        fork -> bad
        ok -> merge
        bad -> merge
        merge -> exit
    "#)),
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        status_of(&report, "merge").as_deref(),
        Some("partial_success")
    );
    assert_eq!(status_of(&report, "bad").as_deref(), Some("failure"));
    let results = published_results(&report);
    assert_eq!(results[0]["status"], json!("succeeded"));
    assert_eq!(results[1]["id"], json!("bad"));
    assert_eq!(results[1]["status"], json!("failed"));
    assert_eq!(results[1]["index"], json!(1));
    // A failed branch keeps its identity and reports what changed: the
    // output it wrote and its failure class, never stale success data.
    assert_eq!(
        results[1]["context_updates"]["command.output"],
        json!("boom\n")
    );
    assert_eq!(
        results[1]["context_updates"]["failure_class"],
        json!("exit_status:3")
    );

    let dir = RunDir::new("parallel-all-failed");
    let rt = runtime(dir.path());
    let report = run(
        &rt,
        lower(&dot(r#"
        fork [shape=component]
        x [shape=parallelogram, script="exit 1"]
        y [shape=parallelogram, script="exit 2"]
        merge [shape=tripleoctagon]
        recover [shape=parallelogram, script="echo recovered"]
        start -> fork
        fork -> x
        fork -> y
        x -> merge
        y -> merge
        merge -> recover [condition="outcome=failed"]
        merge -> exit
        recover -> exit
    "#)),
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "merge").as_deref(), Some("failure"));
    let merge = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "merge")
        .expect("merge ran");
    assert_eq!(
        merge
            .outcome
            .status
            .failure_info()
            .map(|f| f.message.as_str()),
        Some("All parallel branches failed")
    );
    assert_eq!(status_of(&report, "recover").as_deref(), Some("success"));
    assert_eq!(published_results(&report).len(), 2);
}

#[tokio::test]
async fn a_succeed_policy_promotes_a_failed_branch_before_collection() {
    let dir = RunDir::new("parallel-promote");
    let rt = runtime(dir.path());
    let report = run(
        &rt,
        lower(&dot(r#"
        fork [shape=component]
        ok [shape=parallelogram, script="echo fine"]
        soft [shape=parallelogram, script="echo boom; exit 3", on_failure="succeed"]
        merge [shape=tripleoctagon]
        start -> fork
        fork -> ok
        fork -> soft
        ok -> merge
        soft -> merge
        merge -> exit
    "#)),
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let results = published_results(&report);
    assert_eq!(results[1]["id"], json!("soft"));
    assert_eq!(
        results[1]["status"],
        json!("succeeded"),
        "the branch's own policy promoted it before collection"
    );
    assert_eq!(status_of(&report, "merge").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "soft").as_deref(), Some("success"));
}

#[tokio::test]
async fn duplicate_targets_are_separate_branches_with_their_own_index() {
    let dir = RunDir::new("parallel-duplicate");
    let rt = runtime(dir.path());
    let report = run(
        &rt,
        lower(&dot(r#"
        fork [shape=component]
        a [shape=parallelogram, script="echo $RANDOM"]
        merge [shape=tripleoctagon]
        start -> fork
        fork -> a
        fork -> a
        a -> merge
        merge -> exit
    "#)),
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let results = published_results(&report);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], json!("a"));
    assert_eq!(results[1]["id"], json!("a"));
    assert_eq!(results[0]["index"], json!(0));
    assert_eq!(results[1]["index"], json!(1));
    assert_eq!(status_of(&report, "a.branch1").as_deref(), Some("success"));
    assert_eq!(invocation_count(dir.path()), 3);
}

const FOR_EACH: &str = r#"
    plan [shape=parallelogram, output_schema="routing", script="printf '%s' '{\"context_updates\":{\"jobs\":JOBS}}'"]
    fan [shape=component, for_each="context.jobs", max_parallel=2]
    job [prompt="Do the job"]
    join [shape=tripleoctagon]
    report [shape=parallelogram, script="cat", stdin_source="context.parallel.results"]
    start -> plan -> fan -> job -> join -> report -> exit
"#;

#[tokio::test]
async fn an_empty_for_each_list_joins_with_no_branches_and_no_child() {
    let dir = RunDir::new("parallel-empty");
    let rt = runtime(dir.path());
    let report = run(&rt, lower(&dot(&FOR_EACH.replace("JOBS", "[]")))).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(published_results(&report), Vec::<Value>::new());
    assert_eq!(
        report.state.run_context().get("parallel.branch_count"),
        Some(&json!(0))
    );
    assert_eq!(status_of(&report, "join").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "report").as_deref(), Some("success"));
    assert_eq!(output_of(&report, "report")["stdout"], json!("[]\n"));
    assert_eq!(invocation_count(dir.path()), 1, "no child ran");
}

#[tokio::test]
async fn for_each_items_are_labelled_and_ordered_by_index() {
    let dir = RunDir::new("parallel-items");
    let rt = runtime(dir.path());
    let jobs = r#"[{\"name\":\"alpha\"},{\"label\":\"second\"},\"third\"]"#;
    let report = run(&rt, lower(&dot(&FOR_EACH.replace("JOBS", jobs)))).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let results = published_results(&report);
    assert_eq!(results.len(), 3);
    for (index, (envelope, label)) in results.iter().zip(["alpha", "second", "2"]).enumerate() {
        assert_eq!(envelope["id"], json!("job"));
        assert_eq!(envelope["index"], json!(index));
        assert_eq!(envelope["item_label"], json!(label));
        assert_eq!(envelope["status"], json!("succeeded"));
    }
    assert_eq!(invocation_count(dir.path()), 4);
}

#[tokio::test]
async fn a_repeated_fork_publishes_results_per_visit_with_its_own_children() {
    let dir = RunDir::new("parallel-repeat");
    let rt = runtime(dir.path());
    let report = run(
        &rt,
        lower(&dot(r#"
        fork [shape=component]
        a [shape=parallelogram, script="cat round 2>/dev/null || echo first"]
        b [shape=parallelogram, script="echo b"]
        merge [shape=tripleoctagon]
        check [shape=parallelogram, output_schema="routing", script="if [ -f round ]; then printf '%s' '{\"context_updates\":{\"done\":\"yes\"}}'; else echo second > round; printf '%s' '{\"context_updates\":{\"done\":\"no\"}}'; fi"]
        start -> fork
        fork -> a
        fork -> b
        a -> merge
        b -> merge
        merge -> check
        check -> exit [condition="context.done=yes"]
        check -> fork
    "#)),
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let merges = report
        .state
        .history()
        .iter()
        .filter(|r| r.name == "merge")
        .count();
    assert_eq!(merges, 2, "the fork ran twice");
    // The second visit's results stand in the context; the branch `a`
    // saw the workspace the first visit wrote.
    let results = published_results(&report);
    assert_eq!(
        results[0]["context_updates"]["command.output"],
        json!("second\n")
    );
    assert_eq!(invocation_count(dir.path()), 5, "two children per visit");
}

#[tokio::test]
async fn a_nested_fork_runs_inside_its_branch_and_reports_its_own_results() {
    let dir = RunDir::new("parallel-nested");
    let rt = runtime(dir.path());
    let report = run(
        &rt,
        lower(&dot(r#"
        outer [shape=component]
        x [shape=parallelogram, script="echo x"]
        inner [shape=component]
        p [shape=parallelogram, script="echo p"]
        q [shape=parallelogram, script="echo q"]
        inner_join [shape=tripleoctagon]
        outer_join [shape=tripleoctagon]
        start -> outer
        outer -> x
        outer -> inner
        inner -> p
        inner -> q
        p -> inner_join
        q -> inner_join
        x -> outer_join
        inner_join -> outer_join
        outer_join -> exit
    "#)),
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let results = published_results(&report);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], json!("x"));
    assert_eq!(results[1]["id"], json!("inner"));
    assert_eq!(results[1]["status"], json!("succeeded"));
    let inner = &results[1]["context_updates"];
    assert_eq!(inner["parallel.branch_count"], json!(2));
    let inner_results = inner["parallel.results"].as_array().expect("inner results");
    assert_eq!(inner_results[0]["id"], json!("p"));
    assert_eq!(inner_results[1]["id"], json!("q"));
    assert_eq!(
        inner_results[1]["context_updates"]["command.output"],
        json!("q\n")
    );
    // Root, x, inner, p, q.
    assert_eq!(invocation_count(dir.path()), 5);
}

#[tokio::test]
async fn cancelling_the_run_settles_every_branch_child() {
    let dir = RunDir::new("parallel-cancel");
    let rt = runtime(dir.path());
    let workspace = dir.path().join("scopes/invocation-0-scope-0/work");
    let lowered = lower(&dot(r#"
        fork [shape=component]
        a [shape=parallelogram, script="touch started_a; sleep 30; touch finished_a"]
        b [shape=parallelogram, script="touch started_b; sleep 30; touch finished_b"]
        merge [shape=tripleoctagon]
        start -> fork
        fork -> a
        fork -> b
        a -> merge
        b -> merge
        merge -> exit
    "#));
    let report = run_with(&rt, lowered, |handle| {
        let workspace = workspace.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !(workspace.join("started_a").exists() && workspace.join("started_b").exists()) {
                assert!(Instant::now() < deadline, "both branches started");
                thread::sleep(Duration::from_millis(20));
            }
            handle.cancel_root();
        });
    })
    .await;
    assert_eq!(report.status, RunStatus::Cancelled);
    let inspection = inspect_run(dir.path()).expect("inspects");
    assert_eq!(inspection.invocations.len(), 3);
    for invocation in &inspection.invocations {
        assert_eq!(
            invocation.status, "finished",
            "every branch settled before the run ended: {invocation:?}"
        );
    }
    assert!(!workspace.join("finished_a").exists());
    assert!(!workspace.join("finished_b").exists());
    assert_eq!(status_of(&report, "a").as_deref(), Some("cancelled"));
}

/// Keep `lines` of a JSONL file up to and including the first line `keep`
/// accepts.
fn truncate_after(path: &Path, keep: impl Fn(&str) -> bool) {
    let text = fs::read_to_string(path).expect("reads");
    let mut out = Vec::new();
    for line in text.lines() {
        out.push(line);
        if keep(line) {
            break;
        }
    }
    fs::write(path, format!("{}\n", out.join("\n"))).expect("writes");
}

/// Keep the lines of a JSONL file before the first line `cut` accepts.
fn truncate_before(path: &Path, cut: impl Fn(&str) -> bool) {
    let text = fs::read_to_string(path).expect("reads");
    let mut out = Vec::new();
    for line in text.lines() {
        if cut(line) {
            break;
        }
        out.push(line);
    }
    fs::write(path, format!("{}\n", out.join("\n"))).expect("writes");
}

/// A crash after branch `a`'s child finished but before `b`'s did: the
/// resume reattaches `a`'s declared invocation without running it again and
/// finishes `b`.
#[tokio::test]
async fn resume_keeps_a_finished_branch_and_finishes_the_unfinished_one() {
    let dir = RunDir::new("parallel-resume");
    // The workspace outlives the first run: a resume needs the sandbox the
    // crash left behind.
    let rt = runtime_retaining(dir.path(), Retention::Always);
    let text = dot(r#"
        fork [shape=component]
        a [shape=parallelogram, script="echo a >> a_runs; echo a"]
        b [shape=parallelogram, script="sleep 1; echo b >> b_runs; echo b"]
        merge [shape=tripleoctagon]
        start -> fork
        fork -> a
        fork -> b
        a -> merge
        b -> merge
        merge -> exit
    "#);
    let report = run(&rt, lower(&text)).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let inspection = inspect_run(dir.path()).expect("inspects");
    let child_of = |slot: &str| {
        inspection
            .invocations
            .iter()
            .find(|i| i.parent.as_ref().is_some_and(|p| p.slot.contains(slot)))
            .map(|i| i.invocation.raw())
            .expect("the branch child")
    };
    let (child_a, child_b) = (child_of(":a"), child_of(":b"));
    let log_of = |invocation: u64| {
        dir.path().join(format!(
            "invocations/{invocation:016x}/executions/{invocation:016x}/events.jsonl"
        ))
    };
    let a_log_before = fs::read_to_string(log_of(child_a)).expect("a's log");

    // The crash: the coordinator log ends with a's invocation finished; the
    // root log ends with a's branch step finished; b's log has no finish.
    let coordinator = dir.path().join("coordinator.jsonl");
    truncate_after(&coordinator, |line| {
        line.contains("InvocationFinished") && line.contains(&format!("\"invocation\":{child_a}"))
    });
    truncate_after(&log_of(0), |line| {
        line.contains("StepFinished") && line.contains("\"id\":\"a\"")
    });
    truncate_before(&log_of(child_b), |line| line.contains("StepFinished"));

    let rt = runtime_retaining(dir.path(), Retention::Always);
    let resumed = host::resume(&rt).await.expect("resumes");
    assert_eq!(
        resumed.status,
        RunStatus::Success,
        "{:?}",
        resumed.state.errors()
    );
    let inspection = inspect_run(dir.path()).expect("inspects");
    assert_eq!(
        inspection.invocations.len(),
        3,
        "no branch was declared again"
    );
    assert_eq!(
        fs::read_to_string(log_of(child_a)).expect("a's log"),
        a_log_before,
        "the finished branch was not run again"
    );
    let workspace = dir.path().join("scopes/invocation-0-scope-0/work");
    assert_eq!(
        fs::read_to_string(workspace.join("a_runs")).expect("a ran"),
        "a\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("b_runs")).expect("b ran"),
        "b\nb\n"
    );
    let results = published_results(&resumed);
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0]["context_updates"]["command.output"],
        json!("a\n")
    );
    assert_eq!(
        results[1]["context_updates"]["command.output"],
        json!("b\n")
    );
}
