//! The real Fabro steps on the host executor: `fabro/command` with stdin and
//! a routing directive, `fabro/wait`, and `fabro/human` answered through
//! `Control::Deliver` — with byte-identical replay on every run.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fabro_steps::command::OUTPUT_CAP;
use fabro_steps::register;
use frontend_fabro::load;
use runtime::driver::{EventObserver, ExecutionReport, RunHandle};
use runtime::engine::{EngineState, Event, EventRecord, ReplayMismatch};
use runtime::executor::Retention;
use runtime::frontend::{CompileInputs, NoFiles};
use runtime::ir::{CancelScopeId, Graph, RunStatus, TimeoutPolicy};
use runtime::steps::{Answer, Question, Steer};
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
async fn a_command_keeps_only_a_bounded_output_tail() {
    let graph = lower(&dot(r#"
        c [shape=parallelogram, script="printf '%070000d' 0"]
        start -> c -> exit
    "#));
    let report = run(graph, "fabro-command-output-cap").await;
    let result = output_of(&report, "c");
    let output = result["stdout"].as_str().expect("string output");
    assert!(output.starts_with("\n… [output truncated]\n"));
    assert!(output.len() <= OUTPUT_CAP + 24, "{}", output.len());
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
    let graph = lower(&dot(r#"
        a [shape=parallelogram, output_schema="routing", script="echo '{\"context_updates\": {\"payload\": \"from context\"}}'"]
        c [shape=parallelogram, script="cat", stdin_source="context.payload"]
        start -> a -> c -> exit
    "#));
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

/// The `succeed` shim: the failed command keeps its failure on the record as
/// a partial status, reports `succeeded`, and its `outcome=succeeded` edge is
/// taken. REMOVE AFTER 2026-10-04 with the shim.
#[tokio::test]
async fn a_succeed_policy_reports_succeeded_and_keeps_the_failure_on_record() {
    let graph = lower(&dot(r#"
        c [shape=parallelogram, script="echo boom; exit 3", on_failure="succeed"]
        ok [shape=parallelogram, script="true"]
        bad [shape=parallelogram, script="true"]
        start -> c
        c -> ok [condition="outcome=succeeded"]
        c -> bad [condition="outcome=failed"]
        c -> exit
        ok -> exit
        bad -> exit
    "#));
    let report = run(graph, "fabro-command-succeed-shim").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "c").as_deref(), Some("partial_success"));
    let output = output_of(&report, "c");
    assert_eq!(output["outcome"], json!("succeeded"));
    assert_eq!(output["failure_class"], json!("exit_status:3"));
    assert_eq!(status_of(&report, "ok").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "bad"), None);
}

// ── Readiness item 6: deadlines the step owns ─────────────────────────────

/// A command's `timeout` goes to the sandbox as the exec deadline. The
/// provider ends the script; the stage fails with Fabro's `Script timed out`
/// reason and class `timeout`, well before the driver's structural budget.
#[tokio::test]
async fn a_command_deadline_is_enforced_by_the_sandbox() {
    let graph = lower(&dot(r#"
        c [shape=parallelogram, script="echo started; sleep 30", timeout="500ms", on_failure="exit"]
        start -> c -> exit
    "#));
    assert_eq!(
        graph
            .nodes
            .iter()
            .find(|n| n.name == "c")
            .expect("c")
            .budget
            .timeout_policy,
        TimeoutPolicy::HandlerManaged
    );
    let started = Instant::now();
    let report = run(graph, "fabro-command-deadline").await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the deadline ended the script: {:?}",
        started.elapsed()
    );
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(status_of(&report, "c").as_deref(), Some("failure"));
    let output = output_of(&report, "c");
    assert_eq!(output["failure_class"], json!("timeout"));
    let reason = output["failure_reason"].as_str().expect("reason");
    assert!(
        reason.starts_with("Script timed out after 500ms"),
        "{reason}"
    );
    assert!(
        reason.contains("started"),
        "the output tail rides along: {reason}"
    );
}

/// Every question the run asked.
struct CountQuestions(Arc<Mutex<Vec<Question>>>);

impl EventObserver for CountQuestions {
    fn on_record(&self, record: &EventRecord, _: &EngineState) {
        if let Event::StepProgress { ev, .. } = &record.event
            && let Some(question) = Question::from_event(ev)
        {
            self.0.lock().expect("not poisoned").push(question);
        }
    }
}

/// Steers the first question it sees, then answers it `N` a little later.
struct SteerThenAnswer {
    handle: Mutex<Option<RunHandle>>,
}

impl EventObserver for SteerThenAnswer {
    fn on_record(&self, record: &EventRecord, _: &EngineState) {
        let Event::StepProgress { firing, ev } = &record.event else {
            return;
        };
        let Some(question) = Question::from_event(ev) else {
            return;
        };
        let handle = self
            .handle
            .lock()
            .expect("not poisoned")
            .clone()
            .expect("wired");
        let firing = *firing;
        tokio::spawn(async move {
            handle
                .deliver(firing, Steer::new("think harder").to_control())
                .await;
            time::sleep(Duration::from_millis(200)).await;
            handle
                .deliver(
                    firing,
                    Answer::choice("N").for_question(&question.id).to_control(),
                )
                .await;
        });
    }
}

/// A human gate's `timeout` is its answer deadline. Unanswered, it fails
/// with Fabro's retry outcome; `max_retries` asks again and the second
/// question is a new occurrence.
#[tokio::test]
async fn an_unanswered_gate_expires_into_the_retry_outcome() {
    let graph = lower(&dot(r#"
        gate [shape=hexagon, label="Ship it?", timeout="300ms", max_retries=1]
        yes [shape=parallelogram, script="true"]
        start -> gate
        gate -> yes [label="[Y] Yes"]
        yes -> exit
    "#));
    assert_eq!(
        graph
            .nodes
            .iter()
            .find(|n| n.name == "gate")
            .expect("gate")
            .budget
            .timeout_policy,
        TimeoutPolicy::HandlerManaged
    );
    let dir = RunDir::new("fabro-human-expiry");
    let rt = runtime(&dir);
    let asked = Arc::new(Mutex::new(Vec::new()));
    let driver = rt
        .driver(graph.clone())
        .observe(Arc::new(CountQuestions(asked.clone())));
    let started = Instant::now();
    let report = rt
        .run_verified(graph, |_| Ok::<_, ReplayMismatch>(driver))
        .await
        .expect("replay is byte-identical");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(status_of(&report, "gate").as_deref(), Some("failure"));
    let output = output_of(&report, "gate");
    assert_eq!(output["failure_class"], json!("retry_requested"));
    assert_eq!(
        output["failure_reason"],
        json!("human gate timeout, no default")
    );
    assert_eq!(
        status_of(&report, "yes"),
        None,
        "an expired gate never falls through"
    );
    let asked = asked.lock().expect("not poisoned");
    assert_eq!(asked.len(), 2, "the retry asked again");
    assert_eq!(asked[0].timeout_ms, Some(300));
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "gate")
        .expect("recorded");
    assert_eq!(record.attempt.raw(), 2);
}

/// With `human.default_choice`, an expired gate takes the named choice and
/// records `timeout` as the answer, as Fabro does.
#[tokio::test]
async fn an_expired_gate_takes_its_default_choice() {
    let graph = lower(&dot(r#"
        gate [shape=hexagon, label="Ship it?", timeout="300ms", human.default_choice="hold"]
        ship [shape=parallelogram, script="true"]
        hold [shape=parallelogram, script="true"]
        start -> gate
        gate -> ship [label="[Y] Yes"]
        gate -> hold [label="[N] No"]
        ship -> exit
        hold -> exit
    "#));
    let report = run(graph, "fabro-human-default-choice").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "hold").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "ship"), None);
    let context = report.state.run_context();
    assert_eq!(context.get("human.gate.selected"), Some(&json!("N")));
    assert_eq!(
        context.get("human.gate.gate.answer"),
        Some(&json!("timeout"))
    );
    assert_eq!(
        context.get("human.gate.gate.question"),
        Some(&json!("Ship it?"))
    );
}

/// A `review_target` gate reads the target from the context, asks Fabro's
/// review question with the reference attached, and shows the URL in its
/// log. A missing or unsafe target fails the gate before anyone is asked and
/// never echoes the URL.
#[tokio::test]
async fn a_review_target_gate_asks_about_the_validated_reference() {
    let target = r#"{\"context_updates\":{\"review_target\":{\"label\":\"the plan\",\"url\":\"https://quarry.lithos.computer/tmp/abc\",\"kind\":\"document\"}}}"#;
    let graph = lower(&dot(&format!(
        r#"
        prep [shape=parallelogram, output_schema="routing", script="echo '{target}'"]
        gate [shape=hexagon, label="Ship it?", review_target=true]
        yes [shape=parallelogram, script="true"]
        start -> prep -> gate
        gate -> yes [label="[Y] Yes"]
        yes -> exit
    "#
    )));
    let dir = RunDir::new("fabro-human-review-target");
    let rt = runtime(&dir);
    let answerer = Arc::new(Answerer {
        handle: Mutex::new(None),
        answer: Answer::choice("Y"),
        asked:  Mutex::new(Vec::new()),
    });
    let driver = rt.driver(graph.clone()).observe(answerer.clone());
    *answerer.handle.lock().expect("not poisoned") = Some(driver.handle());
    let report = rt
        .run_verified(graph, |_| Ok::<_, ReplayMismatch>(driver))
        .await
        .expect("replay is byte-identical");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let asked = answerer.asked.lock().expect("not poisoned").clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].text,
        "Review the the plan document, then choose the next action."
    );
    let reference = asked[0].reference.as_ref().expect("a reference");
    assert_eq!(reference.label, "the plan");
    assert_eq!(reference.url, "https://quarry.lithos.computer/tmp/abc");
    let log = testkit::log_lines(&report);
    assert!(
        log.iter()
            .any(|line| line.contains("review: the plan <https://quarry.lithos.computer/tmp/abc>")),
        "{log:?}"
    );

    // Missing context: fails before asking.
    let graph = lower(&dot(r#"
        gate [shape=hexagon, label="Ship it?", review_target=true]
        yes [shape=parallelogram, script="true"]
        start -> gate
        gate -> yes [label="[Y] Yes"]
        yes -> exit
    "#));
    let report = run(graph, "fabro-human-review-missing").await;
    assert_eq!(report.status, RunStatus::Failed);
    let output = output_of(&report, "gate");
    assert_eq!(output["failure_class"], json!("review_target"));
    assert_eq!(
        output["failure_reason"],
        json!("Human gate \"gate\" has review_target=true but context.review_target is missing")
    );

    // An unsafe URL is refused without being repeated.
    let unsafe_target = r#"{\"context_updates\":{\"review_target\":{\"label\":\"x\",\"url\":\"javascript:alert(1)\",\"kind\":\"document\"}}}"#;
    let graph = lower(&dot(&format!(
        r#"
        prep [shape=parallelogram, output_schema="routing", script="echo '{unsafe_target}'"]
        gate [shape=hexagon, label="Ship it?", review_target=true]
        yes [shape=parallelogram, script="true"]
        start -> prep -> gate
        gate -> yes [label="[Y] Yes"]
        yes -> exit
    "#
    )));
    let report = run(graph, "fabro-human-review-unsafe").await;
    assert_eq!(report.status, RunStatus::Failed);
    let reason = output_of(&report, "gate")["failure_reason"]
        .as_str()
        .expect("reason")
        .to_owned();
    assert!(reason.contains("invalid context.review_target"), "{reason}");
    assert!(!reason.contains("javascript"), "{reason}");
    // The gate itself never logs the refused URL (the `prep` command's own
    // echo of its directive is the command's output, not the gate's).
    assert!(
        !testkit::log_lines(&report)
            .iter()
            .any(|line| line.starts_with("review:") && line.contains("javascript")),
        "the gate never shows the refused URL"
    );
}

/// A steer delivered to a waiting gate is not an answer: the question stays
/// open until a real answer arrives.
#[tokio::test]
async fn a_steer_does_not_answer_a_gate() {
    let graph = lower(&dot(GATE));
    let dir = RunDir::new("fabro-human-steer");
    let rt = runtime(&dir);
    let answerer = Arc::new(SteerThenAnswer {
        handle: Mutex::new(None),
    });
    let driver = rt.driver(graph.clone()).observe(answerer.clone());
    *answerer.handle.lock().expect("not poisoned") = Some(driver.handle());
    let report = rt
        .run_verified(graph, |_| Ok::<_, ReplayMismatch>(driver))
        .await
        .expect("replay is byte-identical");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "no").as_deref(), Some("success"));
    let deliveries = report
        .state
        .log
        .events()
        .filter(|e| matches!(e, Event::ControlRequested { .. }))
        .count();
    assert_eq!(
        deliveries, 2,
        "the steer and the answer are both in the log"
    );
}
