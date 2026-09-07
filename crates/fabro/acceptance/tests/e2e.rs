//! Acceptance §7 item 6: real Fabro workflows end to end on the host
//! executor, replay verified byte for byte by the runtime.
//!
//! `gh-list` runs its two command nodes against a stub `gh` on `PATH`;
//! `hello` runs its agent node against the fake ACP agent Fabro ships (packaged
//! under `testdata/`); a
//! `for_each` fan-out expands over items a real command produced, under stub
//! agents; a `selection="random"` node routes on a recorded draw. Corpus
//! tests skip when the corpus is not fetched.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, fs};

use execution::host::{self, HostRun};
use execution::inspect::inspect_run;
use fabro_acceptance::runs::fresh_run_dir;
use fabro_acceptance::{corpus_root, has_corpus, lower_one};
use fabro_steps::blobs::{holds_ref, hydrate};
use fabro_steps::{AGENT_KIND, HUMAN_KIND, STAGE_KIND, StubStep, WAIT_KIND, WORKFLOW_KIND};
use frontend::{CompileInputs, NoFiles};
use frontend_fabro::load;
use ir::{ExprOrValue, Graph, RunStatus, Value};
use runtime::driver::ExecutionReport;
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::json;

#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr"
)]
fn corpus() -> Option<PathBuf> {
    let root = corpus_root();
    if has_corpus(&root) {
        return Some(root);
    }
    assert!(
        !env::var("PETRI_REQUIRE_FABRO_CORPUS").is_ok_and(|v| !v.is_empty()),
        "PETRI_REQUIRE_FABRO_CORPUS is set, but the Fabro corpus is not fetched"
    );
    eprintln!("skipping: Fabro corpus not fetched; run scripts/corpus-fetch-fabro.sh");
    None
}

fn options(dir: &Path) -> RunOptions {
    let mut options = RunOptions::new(dir);
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    options
}

/// The real steps.
fn real(dir: &Path) -> Runtime {
    fabro_steps::register(Runtime::standard()).options(options(dir))
}

/// Real commands, stubbed agents: what a fan-out over items needs without a
/// model.
fn commands_and_stubs(dir: &Path) -> Runtime {
    let runtime = Runtime::standard().step(fabro_steps::CommandStep);
    let mut registry = runtime.registry().clone();
    for kind in [AGENT_KIND, HUMAN_KIND, WAIT_KIND, WORKFLOW_KIND, STAGE_KIND] {
        registry.register_runner(Arc::new(StubStep::new(kind)));
    }
    registry.register(fabro_steps::BranchStep);
    registry.register(fabro_steps::FanInStep);
    fabro_steps::services(runtime.steps(registry).options(options(dir)))
}

fn set_env(graph: &mut Graph, pairs: &[(&str, String)]) {
    for scope in &mut graph.body.scopes {
        for (key, value) in pairs {
            scope
                .env
                .insert((*key).into(), ExprOrValue::Value(json!(value)));
        }
    }
}

fn node_config<'a>(graph: &'a mut Graph, name: &str) -> &'a mut Value {
    &mut graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("node `{name}`"))
        .step
        .config
}

async fn run(rt: &Runtime, graph: Graph) -> ExecutionReport {
    rt.run(graph).await.expect("replay is byte-identical")
}

/// A `gh` that records its arguments and answers with a fixed listing.
fn install_gh_stub(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let gh = bin.join("gh");
    fs::write(
        &gh,
        "#!/usr/bin/env bash\necho \"$@\" >> \"$GH_STUB_LOG\"\necho \"#1  stub item  main\"\n",
    )
    .expect("write the stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    bin
}

#[tokio::test]
async fn gh_list_runs_its_commands_against_a_stub_gh() {
    let Some(root) = corpus() else {
        return;
    };
    let (outcome, graph) = lower_one(&root, ".fabro/workflows/gh-list/workflow.fabro");
    let artifact = graph.unwrap_or_else(|| panic!("gh-list lowers: {:?}", outcome.diagnostics));
    assert!(artifact.children.is_empty());
    let mut graph = artifact.graph;
    let dir = fresh_run_dir("fabro-e2e-gh-list");
    let bin = install_gh_stub(&dir);
    let log = dir.join("gh.log");
    let path = format!(
        "{}:{}",
        bin.display(),
        env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );
    set_env(&mut graph, &[
        ("PATH", path),
        ("GH_STUB_LOG", log.display().to_string()),
    ]);
    let report = run(&real(&dir), graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let calls = fs::read_to_string(&log).expect("the stub was called");
    assert!(calls.contains("pr list --state open --limit 50"), "{calls}");
    assert!(
        calls.contains("issue list --state open --limit 50"),
        "{calls}"
    );
    assert_eq!(
        testkit::output_of(&report, "list_issues")["stdout"],
        json!("#1  stub item  main\n")
    );
}

/// The fake ACP agent Fabro ships, from the packaged test data.
fn fake_acp_agent(dir: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/fake_acp_agent.py");
    let script =
        fs::read_to_string(&source).unwrap_or_else(|e| panic!("{}: {e}", source.display()));
    let path = dir.join("fake_acp_agent.py");
    fs::write(&path, script).expect("write the fake agent");
    path
}

#[tokio::test]
async fn hello_runs_its_agent_against_the_fake_acp_agent() {
    let Some(root) = corpus() else {
        return;
    };
    let dir = fresh_run_dir("fabro-e2e-hello");
    let agent = fake_acp_agent(&dir);
    let (outcome, graph) = lower_one(&root, ".fabro/workflows/hello/workflow.fabro");
    let artifact = graph.unwrap_or_else(|| panic!("hello lowers: {:?}", outcome.diagnostics));
    assert!(artifact.children.is_empty());
    let mut graph = artifact.graph;
    // The corpus workflow names no backend, so it lowers to the native
    // agent, as Fabro's own default does. This case is the ACP path: it
    // names the backend and the command the fake agent runs under.
    node_config(&mut graph, "greet")["backend"] = json!("acp");
    node_config(&mut graph, "greet")["acp"] =
        json!({ "command": format!("python3 {}", agent.display()) });
    let report = run(&real(&dir), graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        testkit::output_of(&report, "greet")["text"],
        json!("hello from acp")
    );
    assert_eq!(
        report
            .state
            .history()
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["start", "greet", "exit"]
    );
}

#[tokio::test]
async fn a_for_each_fan_out_expands_over_items_a_command_produced() {
    let text = r#"digraph T {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        plan [shape=parallelogram, output_schema="routing", script="echo '{\"context_updates\": {\"jobs\": [{\"name\": \"a\"}, {\"name\": \"b\"}, {\"name\": \"c\"}]}}'"]
        fan [shape=component, for_each="context.jobs", max_parallel=2]
        job [prompt="Do the job"]
        join [shape=tripleoctagon]
        report [shape=parallelogram, script="cat", stdin_source="context.parallel.results"]
        start -> plan -> fan -> job -> join -> report -> exit
    }"#;
    let lowered = load(
        "fan.fabro",
        text,
        &NoFiles,
        &CompileInputs::new().with_input("item", "x"),
    );
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("{:?}", lowered.diagnostics));
    let dir = fresh_run_dir("fabro-e2e-for-each");
    // Branches run as child invocations, so the fan-out needs the host's
    // coordinator and the lowered child graphs.
    let rt = commands_and_stubs(&dir);
    let report = host::run_configured(
        &rt,
        HostRun::new(graph).with_children(lowered.children),
        |_, _| {},
    )
    .await
    .expect("the run completes");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?} {:?}",
        report.state.errors(),
        report
            .state
            .history()
            .iter()
            .map(|r| (
                r.name.to_string(),
                r.outcome.status.tag(),
                r.outcome.output.clone()
            ))
            .collect::<Vec<_>>()
    );
    let clones = report
        .state
        .history()
        .iter()
        .filter(|r| r.name.starts_with("job"))
        .count();
    assert_eq!(clones, 3, "one clone per item");
    let joined = testkit::output_of(&report, "join");
    let results = joined
        .as_array()
        .expect("the fan-in's output is the ordered branch results");
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r["status"] == json!("succeeded")));
    assert!(results.iter().all(|r| r["id"] == json!("job")));
    let labels: Vec<&Value> = results.iter().map(|r| &r["item_label"]).collect();
    assert_eq!(labels, [&json!("a"), &json!("b"), &json!("c")]);
    let stdout = testkit::output_of(&report, "report")["stdout"]
        .as_str()
        .expect("the report read the results on stdin")
        .to_string();
    assert!(stdout.contains("\"status\""), "{stdout}");
}

#[tokio::test]
async fn random_selection_routes_on_a_recorded_draw() {
    let text = r#"digraph T {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        pick [shape=parallelogram, script="true", selection="random"]
        heads [shape=parallelogram, script="true"]
        tails [shape=parallelogram, script="true"]
        start -> pick
        pick -> heads [weight=1]
        pick -> tails [weight=1]
        heads -> exit
        tails -> exit
    }"#;
    let lowered = load("random.fabro", text, &NoFiles, &CompileInputs::new());
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("{:?}", lowered.diagnostics));
    let dir = fresh_run_dir("fabro-e2e-random");
    // The runtime replays the log after the run: a draw that was not recorded
    // would fail the replay, and `run` would return an error.
    let report = run(&real(&dir), graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let taken: Vec<&str> = report
        .state
        .history()
        .iter()
        .map(|r| r.name.as_str())
        .filter(|n| *n == "heads" || *n == "tails")
        .collect();
    assert_eq!(
        taken.len(),
        1,
        "exactly one arm of the random group: {taken:?}"
    );
}

/// Two successive `for_each` fan-outs over 1,000 items each: 2,001 invocations,
/// well under the run-wide ceiling of 10,000, every branch a durable child
/// invocation. The first fork's 1,000 envelopes exceed the offload threshold,
/// so the second fork's snapshot (copied into each of its 1,000 children)
/// carries a blob reference, not the list.
///
/// Ignored: on 2026-09-07, with live children bounded to `max_parallel`, one
/// 1,000-item fork still had 207 children finished after 280 s at 7.4 GB RSS.
/// The remaining cost is the O(N) fork snapshot copied per child: every
/// child's `InvocationDeclared`, `ExecutionDeclared` and `InvocationFinished`
/// record carries it, as do the resolved config and the waiting branch
/// firing on the parent side. [`fork_scaling_probe`] measures one fork at a
/// chosen size.
#[tokio::test]
#[ignore = "declares 2,001 durable invocations, each copying the O(N) fork snapshot; run by hand"]
async fn two_successive_thousand_item_forks_stay_under_the_ceiling() {
    let text = r#"digraph T {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        plan [shape=parallelogram, output_schema="routing", script="python3 -c 'import json; print(json.dumps({\"context_updates\": {\"jobs\": [{\"name\": \"job-\" + str(i)} for i in range(1000)]}}))'"]
        first [shape=component, for_each="context.jobs", max_parallel=32]
        job [prompt="Do the job"]
        first_join [shape=tripleoctagon]
        second [shape=component, for_each="context.jobs", max_parallel=32]
        again [prompt="Do the job again"]
        second_join [shape=tripleoctagon]
        start -> plan -> first -> job -> first_join -> second -> again -> second_join -> exit
    }"#;
    let lowered = load("forks.fabro", text, &NoFiles, &CompileInputs::new());
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("{:?}", lowered.diagnostics));
    let dir = fresh_run_dir("fabro-e2e-two-thousand");
    let rt = commands_and_stubs(&dir);
    let report = host::run_configured(
        &rt,
        HostRun::new(graph).with_children(lowered.children),
        |_, _| {},
    )
    .await
    .expect("the run completes");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let stored = report
        .state
        .run_context()
        .get("parallel.results")
        .cloned()
        .expect("the second fork's results");
    assert!(
        holds_ref(&stored),
        "1,000 envelopes are above the offload threshold: {stored}"
    );
    let store = fabro_steps::LocalBlobStore::new(dir.join(fabro_steps::BLOBS_DIR));
    let results = hydrate(stored, &store).await;
    let results = results.as_array().expect("the hydrated list");
    assert_eq!(results.len(), 1000);
    assert_eq!(results[999]["item_label"], json!("job-999"));
    assert_eq!(results[999]["index"], json!(999));
    let inspection = inspect_run(&dir).expect("inspects");
    assert_eq!(
        inspection.invocations.len(),
        2001,
        "the root and 2,000 branches"
    );
    // The second fork's children were declared from a snapshot that holds
    // the reference, not the first fork's 1,000 envelopes.
    let second_fork_child = inspection
        .invocations
        .iter()
        .rev()
        .find(|invocation| invocation.invocation.raw() > 1000)
        .expect("a child of the second fork");
    let declared = serde_json::to_string(&second_fork_child.context).expect("json");
    assert!(
        declared.len() < 64 * 1024,
        "the child's declared context is small: {} bytes",
        declared.len()
    );
}

/// One `for_each` fork over `PETRI_FORK_PROBE_ITEMS` items (default 100)
/// under `PETRI_FORK_PROBE_MAX_PARALLEL` slots (default 32): the per-child
/// cost of a fan-out, measured by hand at several sizes with
/// `/usr/bin/time -l`. Prints the wall time and the record sizes.
#[tokio::test]
#[ignore = "a measurement, not a check; run by hand with PETRI_FORK_PROBE_ITEMS"]
#[expect(
    clippy::print_stderr,
    reason = "the probe's measurements are its output"
)]
async fn fork_scaling_probe() {
    let items: usize = env::var("PETRI_FORK_PROBE_ITEMS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let max_parallel: usize = env::var("PETRI_FORK_PROBE_MAX_PARALLEL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let text = format!(
        r#"digraph T {{
        start [shape=Mdiamond]
        exit [shape=Msquare]
        plan [shape=parallelogram, output_schema="routing", script="python3 -c 'import json; print(json.dumps({{\"context_updates\": {{\"jobs\": [{{\"name\": \"job-\" + str(i)}} for i in range({items})]}}}}))'"]
        fan [shape=component, for_each="context.jobs", max_parallel={max_parallel}]
        job [prompt="Do the job"]
        join [shape=tripleoctagon]
        start -> plan -> fan -> job -> join -> exit
    }}"#
    );
    let lowered = load("probe.fabro", &text, &NoFiles, &CompileInputs::new());
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("{:?}", lowered.diagnostics));
    let dir = fresh_run_dir("fabro-e2e-fork-probe");
    let rt = commands_and_stubs(&dir);
    let started = Instant::now();
    let report = host::run_configured(
        &rt,
        HostRun::new(graph).with_children(lowered.children),
        |_, _| {},
    )
    .await
    .expect("the run completes");
    let elapsed = started.elapsed();
    assert_eq!(report.status, RunStatus::Success);
    let coordinator = fs::metadata(dir.join("coordinator.jsonl")).map_or(0, |m| m.len());
    eprintln!(
        "fork probe: {items} items under {max_parallel} slots in {:.1} s ({:.0} ms per child); \
         coordinator.jsonl {} KB",
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / items as f64,
        coordinator / 1024
    );
}
