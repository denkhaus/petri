//! Stage self-awareness in command steps: a command script sees its own
//! meta-level through engine-bound environment variables — the node, the
//! firing, and, when the coordinator drives the run, the run key and the
//! execution. The names mirror the hook vocabulary (`FABRO_RUN_ID`,
//! `FABRO_NODE_ID`); the firing and execution pair is the same stage
//! position a checkpoint or observation record carries, so a script can
//! attribute its own platform records without any convention of its own.
//!
//! Two arms: a run through the coordinator (the embedding host's driver,
//! which registers the `ExecutionIdentity` the run key and execution come
//! from) and a bare driver run, which has no identity — a command there
//! still knows its node and firing, but not a run or an execution, and
//! must not fail for the absence.

use std::time::Duration;

use attractor_steps::register;
use execution::{RunKey, host};
use frontend_attractor::load;
use runtime::executor::Retention;
use runtime::ir::{Graph, RunStatus};
use runtime::{RunOptions, Runtime};
use testkit::{RunDir, output_of};

/// The run key the coordinator names the run by.
const RUN_KEY: &str = "stage-env-run";

fn dot(body: &str) -> String {
    format!("digraph T {{\n  start [shape=Mdiamond]\n  exit [shape=Msquare]\n{body}\n}}")
}

#[expect(
    clippy::print_stderr,
    reason = "a graph that fails to lower explains itself in the test output"
)]
fn lower(text: &str) -> Graph {
    let lowered = load(
        "test.fabro",
        text,
        &frontend::NoFiles,
        &frontend::CompileInputs::new(),
    );
    for diagnostic in lowered.diagnostics.iter() {
        eprintln!("{diagnostic}");
    }
    lowered.graph.expect("lowers")
}

/// A command node that echoes the quartet, one per line, with the firing
/// last so a missing variable leaves an empty line the assertions see.
fn echo_graph() -> Graph {
    lower(&dot(r#"
        c [shape=parallelogram, script="printf '%s\n%s\n%s\n%s\n' \"$FABRO_RUN_ID\" \"$FABRO_NODE_ID\" \"$FABRO_EXECUTION\" \"$FABRO_FIRING\""]
        start -> c -> exit
    "#))
}

fn runtime(dir: &RunDir) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    register(Runtime::standard()).options(options)
}

fn lines(output: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = output.split('\n').collect();
    lines.pop();
    lines
}

/// Through the coordinator the script sees the whole quartet: the run key
/// the host named, its own node, execution 0, and a numeric firing.
#[tokio::test]
async fn a_command_sees_its_stage_identity_under_the_coordinator() {
    let dir = RunDir::new("stage-env-coordinated");
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    options.run_key = Some(RunKey::new(RUN_KEY));
    let rt = register(Runtime::standard()).options(options);
    let report = host::run_configured(&rt, host::HostRun::new(echo_graph()), |_, _| {})
        .await
        .expect("the run completes");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "c");
    let values = lines(output["stdout"].as_str().unwrap_or_default());
    assert_eq!(values.len(), 4, "four lines, one per variable: {values:?}");
    assert_eq!(values[0], RUN_KEY, "FABRO_RUN_ID is the run key");
    assert_eq!(values[1], "c", "FABRO_NODE_ID is the node's own name");
    assert_eq!(values[2], "0", "FABRO_EXECUTION is the execution");
    assert!(
        values[3].chars().all(|c| c.is_ascii_digit()) && !values[3].is_empty(),
        "FABRO_FIRING is a number: {:?}",
        values[3]
    );
}

/// A bare driver has no identity: the node and firing still bind, the run
/// and execution do not, and the command succeeds regardless.
#[tokio::test]
async fn a_command_without_an_identity_binds_only_what_it_has() {
    let dir = RunDir::new("stage-env-bare");
    let report = runtime(&dir)
        .run(echo_graph())
        .await
        .expect("replay is byte-identical");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "c");
    let values = lines(output["stdout"].as_str().unwrap_or_default());
    assert_eq!(values.len(), 4, "four lines, one per variable: {values:?}");
    assert_eq!(values[0], "", "no coordinator, no FABRO_RUN_ID");
    assert_eq!(values[1], "c", "FABRO_NODE_ID binds without an identity");
    assert_eq!(values[2], "", "no coordinator, no FABRO_EXECUTION");
    assert!(
        values[3].chars().all(|c| c.is_ascii_digit()) && !values[3].is_empty(),
        "FABRO_FIRING binds without an identity: {:?}",
        values[3]
    );
}
