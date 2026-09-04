//! Acceptance for `fabro/workflow`: a `house` node's child workflow is
//! lowered with the parent, registered before the run, invoked through the
//! coordinator once per cycle, and stopped by its condition.

use std::path::Path;
use std::time::Duration;

use execution::host::{self, HostRun};
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

#[tokio::test]
async fn a_manager_loop_runs_its_child_until_the_stop_condition_holds() {
    let parent = r#"digraph Parent {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        seed [shape=parallelogram, output_schema="routing", script="echo '{\"context_updates\": {\"n\": \"0\"}}'"]
        loop [shape=house, stack.child_workflow="child.fabro", manager.max_cycles=10, manager.stop_condition="context.n >= 3"]
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
    let dir = fresh_run_dir("fabro-workflow-stop");
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
    assert_eq!(output["cycles"], json!(3));
    assert_eq!(report.state.run_context().get("n"), Some(&json!("3")));
}

#[tokio::test]
async fn an_inline_child_runs_once_by_default_and_a_failing_child_fails_the_node() {
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
    assert_eq!(
        testkit::output_of(&report, "loop")["failure_class"],
        json!("child_failed")
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
