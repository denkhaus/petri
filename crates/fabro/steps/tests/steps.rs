//! The real Fabro steps on the host executor: `fabro/command` with stdin and
//! a routing directive, `fabro/wait`, and `fabro/human` answered through
//! `Control::Deliver` — with byte-identical replay on every run.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fabro_steps::register;
use frontend_fabro::load;
use runtime::driver::{EventObserver, ExecutionReport, RunHandle};
use runtime::engine::{EngineState, Event, EventRecord, ReplayMismatch};
use runtime::executor::Retention;
use runtime::frontend::{CompileInputs, NoFiles};
use runtime::ir::{CancelScopeId, Graph, RunStatus, Value};
use runtime::steps::{Answer, Question};
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, output_of, status_of};
use tokio::time;

fn dot(body: &str) -> String {
    format!("digraph T {{\n  start [shape=Mdiamond]\n  exit [shape=Msquare]\n{body}\n}}")
}

#[expect(
    clippy::print_stderr,
    reason = "a graph that fails to lower explains itself in the test output"
)]
fn lower(text: &str) -> Graph {
    let lowered = load("test.fabro", text, &NoFiles, &CompileInputs::new());
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("lowers")
}

fn runtime(dir: &RunDir) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    register(Runtime::standard()).options(options)
}

/// Script a node's stub, for the kinds that still stub.
fn script_node(graph: &mut Graph, node: &str, value: Value) {
    let node = graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == node)
        .expect("node");
    node.step.config["simulate"] = value;
}

async fn run(graph: Graph, label: &str) -> ExecutionReport {
    let dir = RunDir::new(label);
    let rt = runtime(&dir);
    rt.run(graph).await.expect("replay is byte-identical")
}

#[tokio::test]
async fn a_command_runs_in_bash_and_reports_its_output() {
    let graph = lower(&dot(r#"
        c [shape=parallelogram, script="echo hello; echo err >&2; exit 0"]
        start -> c -> exit
    "#));
    let report = run(graph, "fabro-command").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "c");
    assert_eq!(output["outcome"], json!("succeeded"));
    assert_eq!(output["exit_status"], json!(0));
    assert_eq!(
        output["stdout"],
        json!("hello\nerr\n"),
        "stderr is merged, as Fabro merges it"
    );
    assert_eq!(
        report.state.run_context().get("command.output"),
        Some(&json!("hello\nerr\n"))
    );
}

#[tokio::test]
async fn a_failing_command_fails_with_its_exit_status_and_routes() {
    let graph = lower(&dot(r#"
        c [shape=parallelogram, script="echo boom; exit 3"]
        recover [shape=parallelogram, script="true"]
        start -> c
        c -> recover [condition="outcome=failed"]
        c -> exit
        recover -> exit
    "#));
    let report = run(graph, "fabro-command-fail").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "c").as_deref(), Some("failure"));
    assert_eq!(
        output_of(&report, "c")["failure_class"],
        json!("exit_status:3")
    );
    assert_eq!(status_of(&report, "recover").as_deref(), Some("success"));
}

#[tokio::test]
async fn stdin_source_feeds_the_script_from_the_context() {
    let mut graph = lower(&dot(r#"
        a [prompt="x"]
        c [shape=parallelogram, script="cat", stdin_source="context.payload"]
        start -> a -> c -> exit
    "#));
    script_node(
        &mut graph,
        "a",
        json!({ "context_updates": { "payload": "from context" } }),
    );
    let report = run(graph, "fabro-command-stdin").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "c")["stdout"], json!("from context\n"));
}

#[tokio::test]
async fn a_routing_directive_in_the_output_steers_the_next_edge() {
    let graph = lower(&dot(r#"
        c [shape=parallelogram, output_schema="routing", script="echo working; echo '{\"outcome\": \"succeeded\", \"preferred_next_label\": \"[S] Slow\", \"context_updates\": {\"mode\": \"slow\"}}'"]
        fast [shape=parallelogram, script="true"]
        slow [shape=parallelogram, script="true"]
        start -> c
        c -> fast [label="Fast"]
        c -> slow [label="Slow"]
        fast -> exit
        slow -> exit
    "#));
    let report = run(graph, "fabro-command-directive").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "slow").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "fast"), None);
    assert_eq!(report.state.run_context().get("mode"), Some(&json!("slow")));
}

#[tokio::test]
async fn a_directive_that_reports_failure_fails_the_stage() {
    let graph = lower(&dot(r#"
        c [shape=parallelogram, output_schema="routing", script="echo '{\"outcome\": \"failed\", \"failure_reason\": \"no\"}'"]
        start -> c -> exit
    "#));
    let report = run(graph, "fabro-command-directive-fail").await;
    assert_eq!(status_of(&report, "c").as_deref(), Some("failure"));
    assert_eq!(output_of(&report, "c")["failure_reason"], json!("no"));
    assert_eq!(
        report.status,
        RunStatus::Success,
        "route falls through to exit"
    );
}

#[tokio::test]
async fn wait_sleeps_for_its_duration() {
    let graph = lower(&dot(r#"
        w [shape=insulator, duration="50ms"]
        start -> w -> exit
    "#));
    let report = run(graph, "fabro-wait").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "w")["waited_ms"], json!(50));
}

/// Answers the first question it sees with `answer`, through the run handle.
struct Answerer {
    handle: Mutex<Option<RunHandle>>,
    answer: Answer,
    asked:  Mutex<Vec<Question>>,
}

impl EventObserver for Answerer {
    fn on_record(&self, record: &EventRecord, _state: &EngineState) {
        let Event::StepProgress { firing, ev } = &record.event else {
            return;
        };
        let Some(question) = Question::from_event(ev) else {
            return;
        };
        self.asked
            .lock()
            .expect("not poisoned")
            .push(question.clone());
        let handle = self
            .handle
            .lock()
            .expect("not poisoned")
            .clone()
            .expect("wired");
        let answer = self.answer.clone().for_question(&question.id);
        let firing = *firing;
        tokio::spawn(async move {
            handle.deliver(firing, answer.to_control()).await;
        });
    }
}

const GATE: &str = r#"
    gate [shape=hexagon, label="Ship it?", question_type="yes_no"]
    yes [shape=parallelogram, script="true"]
    no [shape=parallelogram, script="true"]
    free [shape=parallelogram, script="true"]
    start -> gate
    gate -> yes [label="[Y] Yes"]
    gate -> no [label="[N] No"]
    gate -> free [freeform=true]
    yes -> exit
    no -> exit
    free -> exit
"#;

async fn run_gate(answer: Answer, label: &str) -> (ExecutionReport, Vec<Question>) {
    let graph = lower(&dot(GATE));
    let dir = RunDir::new(label);
    let rt = runtime(&dir);
    let answerer = Arc::new(Answerer {
        handle: Mutex::new(None),
        answer,
        asked: Mutex::new(Vec::new()),
    });
    let driver = rt.driver(graph.clone()).observe(answerer.clone());
    *answerer.handle.lock().expect("not poisoned") = Some(driver.handle());
    let report = rt
        .run_verified(graph, |_| Ok::<_, ReplayMismatch>(driver))
        .await
        .expect("replay is byte-identical");
    let asked = answerer.asked.lock().expect("not poisoned").clone();
    (report, asked)
}

#[tokio::test]
async fn a_human_gate_routes_on_the_delivered_choice() {
    let (report, asked) = run_gate(Answer::choice("n"), "fabro-human-choice").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].text, "Ship it?");
    assert_eq!(asked[0].options.len(), 2);
    assert_eq!(asked[0].default.as_deref(), Some("Y"));
    assert!(asked[0].freeform);
    assert_eq!(status_of(&report, "no").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "yes"), None);
    assert_eq!(output_of(&report, "gate")["preferred_label"], json!("No"));
    assert_eq!(
        report.state.run_context().get("human.gate.selected"),
        Some(&json!("N"))
    );
    // The answer is in the log as a delivered control.
    assert!(
        report
            .state
            .log
            .events()
            .any(|e| matches!(e, Event::ControlRequested { .. }))
    );
}

#[tokio::test]
async fn a_human_gate_routes_free_text_to_the_freeform_edge() {
    let (report, _) = run_gate(Answer::text("something else"), "fabro-human-free").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "free").as_deref(), Some("success"));
    assert_eq!(output_of(&report, "gate")["text"], json!("something else"));
}

#[tokio::test]
async fn a_label_answer_matches_without_its_accelerator() {
    let (report, _) = run_gate(Answer::choice("yes"), "fabro-human-label").await;
    assert_eq!(status_of(&report, "yes").as_deref(), Some("success"));
}

#[tokio::test]
async fn a_cancelled_gate_fails_closed() {
    let graph = lower(&dot(r#"
        gate [shape=hexagon, label="Ship it?"]
        yes [shape=parallelogram, script="true"]
        start -> gate
        gate -> yes [label="[Y] Yes"]
        yes -> exit
    "#));
    let dir = RunDir::new("fabro-human-cancel");
    let rt = runtime(&dir);
    let driver = rt.driver(graph.clone());
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    time::sleep(Duration::from_millis(300)).await;
    handle.cancel(CancelScopeId::ROOT).await;
    let report = run.await.expect("the run task");
    assert_ne!(report.status, RunStatus::Success);
    assert_eq!(
        status_of(&report, "yes"),
        None,
        "a failed gate never falls through"
    );
}
