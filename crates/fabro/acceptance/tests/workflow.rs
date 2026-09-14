//! Acceptance for `fabro/workflow`: a `house` node's child workflow is
//! lowered with the parent, registered before the run, invoked through the
//! coordinator once per cycle, and stopped by its condition.

use std::path::Path;
use std::time::Duration;

use execution::host::{self, HostRun};
use execution::inspect::inspect_run_dir;
use fabro_acceptance::runs::fresh_run_dir;
use frontend::{CompileInputs, Lowered, MapFiles, NoFiles};
use frontend_fabro::load;
use ir::RunStatus;
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::json;

fn runtime(dir: &Path) -> Runtime {
    let mut options = RunOptions::new(dir);
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    fabro_steps::register(Runtime::standard()).options(options)
}

#[expect(
    clippy::print_stderr,
    reason = "a graph that fails to lower explains itself in the test output"
)]
fn lowered(text: &str, files: &dyn frontend::FileSource) -> Lowered {
    let lowered = load("parent.fabro", text, files, &CompileInputs::new());
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered
}

const CHILD: &str = r#"digraph Child {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    count [shape=parallelogram, output_schema="routing", script="n=$(cat); n=$((n + 1)); echo \"{\\\"context_updates\\\": {\\\"n\\\": \\\"$n\\\"}}\"", stdin_source="context.n"]
    start -> count -> exit
}"#;

/// Fabro starts one child and polls it. A child that completes before the
/// first poll returns its status and its public context changes, and the
/// manager consumed one child invocation whatever `max_cycles` says.
#[tokio::test]
async fn a_manager_loop_runs_one_child_and_returns_its_context_changes() {
    let parent = r#"digraph Parent {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        seed [shape=parallelogram, output_schema="routing", script="echo '{\"context_updates\": {\"n\": \"0\"}}'"]
        loop [shape=house, stack.child_workflow="child.fabro", manager.max_cycles=1000, manager.stop_condition="context.n >= 3"]
        start -> seed -> loop -> exit
    }"#;
    let files = MapFiles(
        [("child.fabro".to_string(), CHILD.to_string())]
            .into_iter()
            .collect(),
    );
    let lowered = lowered(parent, &files);
    let graph = lowered.graph.expect("the parent lowers");
    assert_eq!(
        lowered.children.len(),
        1,
        "the child was lowered with the parent"
    );
    let dir = fresh_run_dir("fabro-workflow-child");
    let rt = runtime(&dir);
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
    let output = testkit::output_of(&report, "loop");
    assert_eq!(
        output["cycles"],
        json!(1),
        "the child finished before a poll"
    );
    assert_eq!(
        output["notes"],
        json!("Child completed at cycle 1"),
        "{output}"
    );
    assert_eq!(report.state.run_context().get("n"), Some(&json!("1")));
    let inspection = inspect_run_dir(&dir).await.expect("the run inspects");
    assert_eq!(
        inspection.invocations.len(),
        2,
        "the root and exactly one child invocation"
    );
}

/// A failing child fails the manager node with the child's failure; the
/// class is the child's own, and the explicit failure edge routes.
#[tokio::test]
async fn a_failing_child_fails_the_node_with_its_own_failure() {
    let parent = r#"digraph Parent {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        loop [shape=house, stack.child_dot_source="digraph C { start [shape=Mdiamond] exit [shape=Msquare] boom [shape=parallelogram, script=\"exit 2\", on_failure=\"exit\"] start -> boom -> exit }"]
        recover [shape=parallelogram, script="true"]
        start -> loop
        loop -> recover [condition="outcome=failed"]
        loop -> exit
        recover -> exit
    }"#;
    let lowered = lowered(parent, &NoFiles);
    let graph = lowered.graph.expect("the parent lowers");
    assert_eq!(
        graph
            .nodes
            .iter()
            .find(|n| n.name == "loop")
            .expect("the manager node")
            .step
            .config["max_cycles"],
        json!(1000),
        "a missing `manager.max_cycles` is 1000, as in Fabro"
    );
    let dir = fresh_run_dir("fabro-workflow-fail");
    let rt = runtime(&dir);
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
    assert_eq!(
        testkit::status_of(&report, "loop").as_deref(),
        Some("failure")
    );
    let output = testkit::output_of(&report, "loop");
    assert_eq!(output["failure_class"], json!("exit_status:2"));
    assert!(
        output["failure_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("status 2")),
        "the child's failure detail is kept: {output}"
    );
    assert_eq!(
        testkit::status_of(&report, "recover").as_deref(),
        Some("success"),
        "{}",
        testkit::output_of(&report, "recover")
    );
}

#[test]
fn a_missing_child_and_a_cycle_are_specific_rejections() {
    let missing = lowered(
        r#"digraph P { start [shape=Mdiamond] exit [shape=Msquare] m [shape=house, stack.child_workflow="nope.fabro"] start -> m -> exit }"#,
        &NoFiles,
    );
    assert!(
        missing
            .diagnostics
            .iter()
            .any(|d| d.code == "fabro.child_workflow_not_found")
    );
    let cyclic = r#"digraph P { start [shape=Mdiamond] exit [shape=Msquare] m [shape=house, stack.child_workflow="self.fabro"] start -> m -> exit }"#;
    let files = MapFiles(
        [("self.fabro".to_string(), cyclic.to_string())]
            .into_iter()
            .collect(),
    );
    let cycle = load("self.fabro", cyclic, &files, &CompileInputs::new());
    assert!(
        cycle
            .diagnostics
            .iter()
            .any(|d| d.code == "fabro.workflow_cycle"),
        "{:?}",
        cycle.diagnostics
    );
}
