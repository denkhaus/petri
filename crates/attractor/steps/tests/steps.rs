//! The real Fabro steps on the host executor: `attractor/command` with stdin
//! and a routing directive, `attractor/wait`, and `attractor/human` answered
//! through `Control::Deliver` — with byte-identical replay on every run.

use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use attractor_steps::command::OUTPUT_CAP;
use attractor_steps::{LocalBlobStore, blobs, register};
use frontend_attractor::load;
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

/// Output above Fabro's 100 KiB offload threshold leaves the context and the
/// record as a durable `blob://sha256/…` reference; a later command that
/// reads it through `stdin_source` gets the logical value back. Output at
/// the threshold stays inline.
#[tokio::test]
async fn large_command_output_is_offloaded_and_reads_back_logically() {
    let graph = lower(&dot(r#"
        big [shape=parallelogram, script="yes 0123456789 | head -n 20000"]
        count [shape=parallelogram, script="wc -c | tr -d ' '", stdin_source="context.command.output"]
        start -> big -> count -> exit
    "#));
    let dir = RunDir::new("fabro-command-offload");
    let rt = runtime(&dir);
    let report = rt.run(graph).await.expect("replay is byte-identical");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let recorded = output_of(&report, "big")["stdout"]
        .as_str()
        .expect("string output")
        .to_string();
    assert!(
        recorded.starts_with("blob://sha256/"),
        "the record holds a reference, not 200 KiB: {}",
        &recorded[..recorded.len().min(80)]
    );
    let digest = recorded.trim_start_matches("blob://sha256/");
    let stored = dir.path().join("blobs").join(digest);
    assert_eq!(
        fs::read(&stored).expect("the blob file").len(),
        220_000,
        "the store holds the whole output"
    );
    assert_eq!(
        output_of(&report, "count")["stdout"],
        json!("220000\n"),
        "the next command read the logical value"
    );
    // The reference is durable: a fresh store over the same run directory,
    // as a resumed run opens, hydrates it to the same logical value.
    let reopened = LocalBlobStore::new(dir.path().join("blobs"));
    let hydrated = blobs::hydrate(json!(recorded), &reopened).await;
    assert_eq!(
        hydrated.as_str().map(str::len),
        Some(220_000),
        "the logical value reads back after reopening the store"
    );
    let kv_value = report
        .state
        .run_context()
        .get("command.output")
        .cloned()
        .expect("the last command's output");
    assert_eq!(kv_value, json!("220000\n"));
    let small = lower(&dot(r#"
        s [shape=parallelogram, script="printf '%01000d' 7"]
        start -> s -> exit
    "#));
    let report = run(small, "fabro-command-inline").await;
    let inline = output_of(&report, "s")["stdout"]
        .as_str()
        .expect("string")
        .to_string();
    assert_eq!(inline.trim_end().len(), 1000, "a small output stays inline");
}

/// The in-memory cap sits above the offload threshold, so a value the store
/// takes was never truncated first.
const _: () = assert!(OUTPUT_CAP > 200_000);

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
    // `cat` of the context string, byte for byte: no newline the script did
    // not write.
    assert_eq!(output_of(&report, "c")["stdout"], json!("from context"));
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
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _state: &EngineState) {
        let Event::StepProgressRecorded { firing, ev } = &record.event else {
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
    run_gate_on(GATE, answer, label).await
}

/// Run the gate graph `body` with `answer` delivered to its first question;
/// the report and every question asked.
async fn run_gate_on(body: &str, answer: Answer, label: &str) -> (ExecutionReport, Vec<Question>) {
    let graph = lower(&dot(body));
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

/// What a host shows beside the question: each choice's `human.description`
/// and `human.preview` from its edge, and the previous stage's response as
/// the question's context, as Fabro's gate shows it. A choice whose edge
/// says nothing carries neither; a gate with no response before it has no
/// context.
#[tokio::test]
async fn a_human_gate_carries_choice_descriptions_previews_and_its_context() {
    const DESCRIBED: &str = r#"
        plan [shape=parallelogram, output_schema="routing", script="echo '{\"outcome\": \"succeeded\", \"context_updates\": {\"last_stage\": \"plan\", \"response.plan\": \"  Ship the fix in one commit.  \"}}'"]
        gate [shape=hexagon, label="Deploy?"]
        yes [shape=parallelogram, script="true"]
        no [shape=parallelogram, script="true"]
        start -> plan -> gate
        gate -> yes [label="[Y] Yes", "human.description"="Merge and deploy to production", "human.preview"="deploy --prod"]
        gate -> no [label="[N] No"]
        yes -> exit
        no -> exit
    "#;
    let (report, asked) =
        run_gate_on(DESCRIBED, Answer::choice("Y"), "fabro-human-described").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(asked.len(), 1);
    let question = &asked[0];
    assert_eq!(
        question.context.as_deref(),
        Some("Ship the fix in one commit."),
        "the context is the last stage's response, trimmed"
    );
    assert_eq!(question.options.len(), 2);
    assert_eq!(question.options[0].key, "Y");
    assert_eq!(
        question.options[0].description.as_deref(),
        Some("Merge and deploy to production")
    );
    assert_eq!(
        question.options[0].preview.as_deref(),
        Some("deploy --prod")
    );
    assert_eq!(question.options[1].key, "N");
    assert_eq!(question.options[1].description, None);
    assert_eq!(question.options[1].preview, None);
    // The fields are in the recorded question too, absent where unset.
    let recorded = report
        .state
        .log
        .events()
        .find_map(|event| match event {
            Event::StepProgressRecorded { ev, .. } => Question::from_event(ev),
            _ => None,
        })
        .expect("the question is in the log");
    assert_eq!(&recorded, question);
    assert_eq!(status_of(&report, "yes").as_deref(), Some("success"));

    let (_, asked) = run_gate(Answer::choice("n"), "fabro-human-no-context").await;
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].context, None, "no stage before the gate responded");
    assert!(asked[0].options.iter().all(|o| o.description.is_none()));
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

/// Fabro's promotion order: an explicit `outcome=failed` edge on a `succeed`
/// node is taken and the failure stays a failure; without a matching
/// explicit route the failure is promoted, keeps its evidence on the record
/// as a partial status, reports `succeeded`, and the `outcome=succeeded` edge
/// is taken.
#[tokio::test]
async fn succeed_promotes_only_a_failure_no_explicit_route_matches() {
    let explicit = lower(&dot(r#"
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
    let report = run(explicit, "fabro-command-succeed-explicit").await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "c").as_deref(), Some("failure"));
    assert_eq!(output_of(&report, "c")["outcome"], json!("failed"));
    assert_eq!(status_of(&report, "bad").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "ok"), None);

    let promoted = lower(&dot(r#"
        c [shape=parallelogram, script="echo boom; exit 3", on_failure="succeed"]
        ok [shape=parallelogram, script="true"]
        start -> c
        c -> ok [condition="outcome=succeeded"]
        c -> exit
        ok -> exit
    "#));
    let report = run(promoted, "fabro-command-succeed-promoted").await;
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
    assert!(
        output["promoted"]
            .as_str()
            .is_some_and(|note| note.contains("promoted")),
        "{output}"
    );
    assert_eq!(status_of(&report, "ok").as_deref(), Some("success"));
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
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _: &EngineState) {
        if let Event::StepProgressRecorded { ev, .. } = &record.event
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
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _: &EngineState) {
        let Event::StepProgressRecorded { firing, ev } = &record.event else {
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
