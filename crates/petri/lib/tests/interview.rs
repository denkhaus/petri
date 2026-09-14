//! The interview dispatcher against real Fabro human gates on the standalone
//! host: correlation, out-of-order answers from parallel gates, sensitive
//! masking, a failing interviewer, cancellation, expiry, and the receipt.

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use petri::execution::events::{
    CollectingSink, Derived, EventProjector, Parsed, RunEvent, ViewEvent, WaitState, replay_run,
};
use petri::execution::host::{self, HostRun};
use petri::execution::{
    Delivery, ExecutionObserver, InterviewDispatcher, InterviewError, InterviewReceipt,
    InterviewReply, InterviewRequest, Interviewer, ReplyRecord,
};
use petri::executor::Retention;
use petri::frontend::{CompileInputs, Lowered};
use petri::ir::{FailureClass, RunStatus, Status};
use petri::steps::{Answer, QuestionExpired};
use petri::{RunOptions, Runtime, driver};
use serde_json::json;
use testkit::RunDir;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;

/// What a test interviewer replies for one node's question.
type Reply = Box<dyn Fn(&InterviewRequest) -> InterviewReply + Send + Sync>;

fn runtime(dir: &RunDir) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    petri::runtime().options(options)
}

fn lower(rt: &Runtime, dir: &RunDir, text: &str) -> Lowered {
    let path = dir.path().join("wf.fabro");
    fs::write(&path, text).expect("write the workflow");
    rt.check(&path, None, None, &CompileInputs::new())
        .expect("loads")
}

/// A test interviewer: answers by node name.
struct ByNode {
    answers: BTreeMap<&'static str, Reply>,
    seen:    Mutex<Vec<InterviewRequest>>,
    /// Hold the reply to the first node until the second was asked.
    after:   Option<(&'static str, &'static str)>,
}

#[async_trait::async_trait]
impl Interviewer for ByNode {
    async fn reply(&self, request: InterviewRequest, cancel: CancellationToken) -> InterviewReply {
        self.seen
            .lock()
            .expect("not poisoned")
            .push(request.clone());
        if let Some((held, until)) = self.after
            && request.node == held
        {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if self
                    .seen
                    .lock()
                    .expect("not poisoned")
                    .iter()
                    .any(|seen| seen.node == until)
                {
                    break;
                }
                tokio::select! {
                    () = sleep(Duration::from_millis(5)) => {},
                    () = cancel.cancelled() => return InterviewReply::Cancelled,
                }
                assert!(Instant::now() < deadline, "`{until}` never asked");
            }
        }
        match self.answers.get(request.node.as_str()) {
            Some(answer) => answer(&request),
            None => InterviewReply::Failed(InterviewError::new(format!(
                "unexpected question from `{}`",
                request.node
            ))),
        }
    }
}

async fn run(
    rt: &Runtime,
    lowered: Lowered,
    interviewer: Arc<dyn Interviewer>,
) -> (driver::ExecutionReport, InterviewReceipt) {
    let (report, receipt, _) = run_projected(rt, lowered, interviewer).await;
    (report, receipt)
}

/// Run with the public event stream collected beside the receipt.
async fn run_projected(
    rt: &Runtime,
    lowered: Lowered,
    interviewer: Arc<dyn Interviewer>,
) -> (driver::ExecutionReport, InterviewReceipt, Vec<RunEvent>) {
    let dispatcher = InterviewDispatcher::new(interviewer);
    let sink = Arc::new(CollectingSink::default());
    let projector = EventProjector::new(sink.clone());
    let host_run = HostRun::new(lowered.graph.expect("lowers"))
        .with_children(lowered.children)
        .observe(Arc::new(dispatcher.clone()))
        .observe(projector.clone() as Arc<dyn ExecutionObserver>);
    let report = host::run_configured(rt, host_run, |handle, secrets| {
        dispatcher.wire(handle, secrets);
    })
    .await
    .expect("the run completes");
    let receipt = dispatcher.shutdown().await;
    let projection = projector.shutdown().await;
    assert!(projection.is_clean(), "{projection:?}");
    (report, receipt, sink.events())
}

/// The failure class of `node`'s final attempt, when it failed.
fn failure_class(report: &driver::ExecutionReport, node: &str) -> Option<FailureClass> {
    report
        .state
        .history()
        .iter()
        .find(|record| record.name == node)
        .and_then(|record| match &record.outcome.status {
            Status::Failure(info) => Some(info.class.clone()),
            _ => None,
        })
}

/// The names of the nodes that ran, in completion order.
fn ran(report: &driver::ExecutionReport) -> Vec<&str> {
    report
        .state
        .history()
        .iter()
        .map(|record| record.name.as_str())
        .collect()
}

/// The `question_expired` events of a stream.
fn expiries(events: &[RunEvent]) -> Vec<&RunEvent> {
    events
        .iter()
        .filter(|event| matches!(event.parsed(), Some(Parsed::QuestionExpired { .. })))
        .collect()
}

/// Two human gates as the branches of one parallel node. A branch runs its
/// target only and returns to the join, as Fabro runs it, so each gate's
/// answer is read from its branch result rather than from a node it routes
/// to.
const TWO_GATES: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    fan [shape=component]
    a [shape=hexagon, label="A?", question_type="yes_no"]
    b [shape=hexagon, label="B?", question_type="yes_no"]
    join [shape=tripleoctagon]
    report [shape=parallelogram, script="cat", stdin_source="context.parallel.results"]
    start -> fan
    fan -> a
    fan -> b
    a -> join [label="[Y] Yes"]
    a -> join [label="[N] No"]
    b -> join [label="[Y] Yes"]
    b -> join [label="[N] No"]
    join -> report -> exit
}"#;

#[tokio::test]
async fn parallel_gates_are_answered_out_of_order_and_each_answer_lands_on_its_own_firing() {
    let dir = RunDir::new("interview-parallel");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, TWO_GATES);
    let mut answers: BTreeMap<&'static str, Reply> = BTreeMap::new();
    answers.insert(
        "a",
        Box::new(|_| InterviewReply::Answered(Answer::choice("N"))),
    );
    answers.insert(
        "b",
        Box::new(|_| InterviewReply::Answered(Answer::choice("Y"))),
    );
    let interviewer = Arc::new(ByNode {
        answers,
        seen: Mutex::new(Vec::new()),
        // `a` is asked first but answered after `b`.
        after: Some(("a", "b")),
    });
    let (report, receipt) = run(&rt, lowered, interviewer.clone()).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    // Each gate's answer lands in its own branch result, in branch order.
    let results = report
        .state
        .run_context()
        .get("parallel.results")
        .and_then(|value| value.as_array())
        .cloned()
        .expect("the fan-in published the branch results");
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0]["id"], json!("a"));
    assert_eq!(
        results[0]["context_updates"]["human.gate.selected"],
        json!("N")
    );
    assert_eq!(results[1]["id"], json!("b"));
    assert_eq!(
        results[1]["context_updates"]["human.gate.selected"],
        json!("Y")
    );
    assert!(
        !report
            .state
            .run_context()
            .kv
            .contains_key("human.gate.selected"),
        "a branch's answer never reaches the parent context"
    );
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    assert_eq!(receipt.questions.len(), 2);
    for record in &receipt.questions {
        assert_eq!(record.delivery, Delivery::Delivered);
        // Each gate ran in its branch's child invocation, whose call slot
        // names the fork, the fork's firing (its occurrence), the branch
        // index and the target.
        assert!(
            record.invocation_path.starts_with("/branch:fan@"),
            "{}",
            record.invocation_path
        );
        assert!(
            record.invocation_path.ends_with(&format!(
                ":{}:{}",
                i32::from(record.node != "a"),
                record.node
            )),
            "{}",
            record.invocation_path
        );
        assert_eq!(record.occurrence, 1);
        assert_eq!(record.ask, 1);
        assert_eq!(record.kind.as_deref(), Some("yes_no"));
    }
    let seen = interviewer.seen.lock().expect("not poisoned").clone();
    assert_eq!(seen.len(), 2);
    // Each gate ran in its own branch execution; the firing identity a
    // reply binds to is the execution and the firing together.
    assert_ne!(seen[0].execution, seen[1].execution);
    assert_ne!(
        (seen[0].execution, seen[0].firing),
        (seen[1].execution, seen[1].firing)
    );
}

const SENSITIVE_GATE: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Token?", question_type="freeform", sensitive=true]
    use_it [shape=parallelogram, script="echo got-it"]
    start -> gate
    gate -> use_it [freeform=true]
    use_it -> exit
}"#;

#[tokio::test]
async fn a_sensitive_answer_is_registered_first_and_recorded_only_as_its_reference() {
    let dir = RunDir::new("interview-sensitive");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, SENSITIVE_GATE);
    let mut answers: BTreeMap<&'static str, Reply> = BTreeMap::new();
    answers.insert(
        "gate",
        Box::new(|_| InterviewReply::Answered(Answer::text("hunter2-secret-value"))),
    );
    let interviewer = Arc::new(ByNode {
        answers,
        seen: Mutex::new(Vec::new()),
        after: None,
    });
    let (report, receipt) = run(&rt, lowered, interviewer).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    let record = &receipt.questions[0];
    assert!(record.sensitive);
    let ReplyRecord::Answered { text, .. } = &record.reply else {
        panic!("answered: {:?}", record.reply);
    };
    assert_eq!(
        *text,
        Some(json!({ "$secret": format!("answer:{}", record.question) }))
    );
    let rendered = serde_json::to_string(&receipt).expect("encodes");
    assert!(!rendered.contains("hunter2"), "{rendered}");
    let log = serde_json::to_string(&report.state.log).expect("encodes");
    assert!(
        !log.contains("hunter2"),
        "the plaintext never enters the log"
    );
    let context = serde_json::to_string(report.state.run_context().kv.as_ref()).expect("encodes");
    assert!(!context.contains("hunter2"), "{context}");
}

const ONE_GATE: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Go?", question_type="yes_no"]
    yes [shape=parallelogram, script="echo yes"]
    no [shape=parallelogram, script="echo no"]
    start -> gate
    gate -> yes [label="[Y] Yes"]
    gate -> no [label="[N] No"]
    yes -> exit
    no -> exit
}"#;

#[tokio::test]
async fn an_interviewer_failure_fails_the_gate_closed_and_reaches_the_receipt() {
    let dir = RunDir::new("interview-failed");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, ONE_GATE);
    let interviewer = Arc::new(ByNode {
        answers: BTreeMap::new(),
        seen:    Mutex::new(Vec::new()),
        after:   None,
    });
    let (report, receipt) = run(&rt, lowered, interviewer).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert!(!receipt.is_clean());
    assert!(
        receipt.errors[0].contains("unexpected question from `gate`"),
        "{:?}",
        receipt.errors
    );
    assert!(matches!(
        receipt.questions[0].reply,
        ReplyRecord::Failed { .. }
    ));
    assert_eq!(receipt.questions[0].delivery, Delivery::Delivered);
    let ran: Vec<_> = report
        .state
        .history()
        .iter()
        .map(|record| record.name.as_str())
        .collect();
    assert!(!ran.contains(&"yes") && !ran.contains(&"no"), "{ran:?}");
}

/// Never answers; keeps the cancel token so the test can see it fire.
struct Silent(Arc<Mutex<Option<CancellationToken>>>);

#[async_trait::async_trait]
impl Interviewer for Silent {
    async fn reply(&self, _request: InterviewRequest, cancel: CancellationToken) -> InterviewReply {
        *self.0.lock().expect("not poisoned") = Some(cancel.clone());
        cancel.cancelled().await;
        InterviewReply::Cancelled
    }
}

#[tokio::test]
async fn a_cancelled_run_ends_the_pending_wait_and_shutdown_leaves_no_task_behind() {
    let dir = RunDir::new("interview-cancel");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, ONE_GATE);
    let token: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let dispatcher = InterviewDispatcher::new(Arc::new(Silent(token.clone())));
    let host_run = HostRun::new(lowered.graph.expect("lowers"))
        .with_children(lowered.children)
        .observe(Arc::new(dispatcher.clone()));
    let asked = token.clone();
    let mut cancel = None;
    let report = host::run_configured(&rt, host_run, |handle, secrets| {
        dispatcher.wire(handle.clone(), secrets);
        // Cancel once the question has reached the interviewer, the way a
        // person does at the prompt.
        cancel = Some(tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(10);
            while asked.lock().expect("not poisoned").is_none() {
                assert!(Instant::now() < deadline, "the gate never asked");
                sleep(Duration::from_millis(10)).await;
            }
            handle.cancel_root();
        }));
    })
    .await
    .expect("the run completes");
    if let Some(cancel) = cancel {
        cancel.await.expect("the cancel task");
    }
    let receipt = timeout(Duration::from_secs(10), dispatcher.shutdown())
        .await
        .expect("shutdown is bounded");
    assert_eq!(report.status, RunStatus::Cancelled);
    let token = token
        .lock()
        .expect("not poisoned")
        .clone()
        .expect("the question reached the interviewer");
    assert!(token.is_cancelled(), "the pending wait was ended");
    assert_eq!(receipt.questions.len(), 1);
    assert!(matches!(
        receipt.questions[0].delivery,
        Delivery::Late | Delivery::Shutdown
    ));
    assert!(
        !receipt
            .errors
            .iter()
            .any(|error| error.contains("still pending")),
        "{:?}",
        receipt.errors
    );
}

// ── Readiness item 6: identity across loops, nesting, re-asks and expiry ────

/// A gate inside a loop asks once per firing: the second question is the
/// same node's second occurrence, each with `ask: 1`.
#[tokio::test]
async fn a_repeated_gate_in_a_loop_advances_its_occurrence() {
    const LOOPED: &str = r#"digraph G {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        gate [shape=hexagon, label="Again?", max_visits=3]
        again [shape=parallelogram, script="echo again"]
        start -> gate
        gate -> again [label="[A] Again"]
        gate -> exit [label="[D] Done"]
        again -> gate
    }"#;
    let dir = RunDir::new("interview-occurrence");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, LOOPED);
    let count = Arc::new(Mutex::new(0_u32));
    let mut answers: BTreeMap<&'static str, Reply> = BTreeMap::new();
    let counter = count.clone();
    answers.insert(
        "gate",
        Box::new(move |_| {
            let mut count = counter.lock().expect("not poisoned");
            *count += 1;
            InterviewReply::Answered(Answer::choice(if *count == 1 { "A" } else { "D" }))
        }),
    );
    let interviewer = Arc::new(ByNode {
        answers,
        seen: Mutex::new(Vec::new()),
        after: None,
    });
    let (report, receipt) = run(&rt, lowered, interviewer).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    let occurrences: Vec<(u32, u32)> = receipt
        .questions
        .iter()
        .map(|q| (q.occurrence, q.ask))
        .collect();
    assert_eq!(occurrences, [(1, 1), (2, 1)]);
    assert_ne!(receipt.questions[0].firing, receipt.questions[1].firing);
    assert_ne!(receipt.questions[0].question, receipt.questions[1].question);
}

/// A gate inside a nested workflow carries the nested invocation's path,
/// `/<node>` (the manager loop's one durable call site per attempt), and its
/// own invocation id.
#[tokio::test]
async fn a_nested_gate_carries_its_invocation_path() {
    const NESTED: &str = r#"digraph P {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        m [shape=house, stack.child_dot_source="digraph C { start [shape=Mdiamond] exit [shape=Msquare] inner [shape=hexagon, label=\"Inner?\"] ok [shape=parallelogram, script=\"echo ok\"] start -> inner inner -> ok [label=\"[Y] Yes\"] ok -> exit }"]
        outer [shape=hexagon, label="Outer?"]
        start -> m -> outer
        outer -> exit [label="[Y] Yes"]
    }"#;
    let dir = RunDir::new("interview-nested");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, NESTED);
    let mut answers: BTreeMap<&'static str, Reply> = BTreeMap::new();
    answers.insert(
        "inner",
        Box::new(|_| InterviewReply::Answered(Answer::choice("Y"))),
    );
    answers.insert(
        "outer",
        Box::new(|_| InterviewReply::Answered(Answer::choice("Y"))),
    );
    let interviewer = Arc::new(ByNode {
        answers,
        seen: Mutex::new(Vec::new()),
        after: None,
    });
    let (report, receipt) = run(&rt, lowered, interviewer).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    let inner = receipt
        .questions
        .iter()
        .find(|q| q.node == "inner")
        .expect("the inner gate");
    let outer = receipt
        .questions
        .iter()
        .find(|q| q.node == "outer")
        .expect("the outer gate");
    assert_eq!(inner.invocation_path, "/m");
    assert_eq!(outer.invocation_path, "/");
    assert_ne!(inner.invocation, outer.invocation);
    assert_ne!(inner.execution, outer.execution);
}

/// An answer the gate rejects is re-asked as the same occurrence with
/// `ask: 2`; the second reply routes.
#[tokio::test]
async fn an_invalid_answer_is_re_asked_with_the_next_ask_count() {
    let dir = RunDir::new("interview-reask");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, ONE_GATE);
    let mut answers: BTreeMap<&'static str, Reply> = BTreeMap::new();
    answers.insert(
        "gate",
        Box::new(|request| {
            InterviewReply::Answered(Answer::choice(if request.ask == 1 { "maybe" } else { "N" }))
        }),
    );
    let interviewer = Arc::new(ByNode {
        answers,
        seen: Mutex::new(Vec::new()),
        after: None,
    });
    let (report, receipt) = run(&rt, lowered, interviewer).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    let asks: Vec<(u32, u32, &str)> = receipt
        .questions
        .iter()
        .map(|q| (q.occurrence, q.ask, q.question.as_str()))
        .collect();
    assert_eq!(asks.len(), 2, "{asks:?}");
    assert_eq!((asks[0].0, asks[0].1), (1, 1));
    assert_eq!((asks[1].0, asks[1].1), (1, 2));
    assert_eq!(asks[0].2, asks[1].2, "the same question id, asked again");
    let ran: Vec<_> = report
        .state
        .history()
        .iter()
        .map(|record| record.name.as_str())
        .collect();
    assert!(ran.contains(&"no"), "{ran:?}");
}

/// A gate with a 300 ms answer deadline and a default choice.
const TIMED: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Go?", timeout="300ms", human.default_choice="no"]
    yes [shape=parallelogram, script="echo yes"]
    no [shape=parallelogram, script="echo no"]
    start -> gate
    gate -> yes [label="[Y] Yes"]
    gate -> no [label="[N] No"]
    yes -> exit
    no -> exit
}"#;

/// The same gate with no default: an expiry is Fabro's retry outcome.
const TIMED_NO_DEFAULT: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Go?", timeout="300ms"]
    yes [shape=parallelogram, script="echo yes"]
    start -> gate
    gate -> yes [label="[Y] Yes"]
    yes -> exit
}"#;

/// The one expiry of a run: the public event, its subject, and the same
/// event derived again from the run dir.
fn the_expiry(dir: &RunDir, events: &[RunEvent]) -> RunEvent {
    let expired = expiries(events);
    assert_eq!(expired.len(), 1, "one expiry: {expired:?}");
    let event = expired[0];
    let subject = event.subject.as_ref().expect("a firing's event");
    assert_eq!(subject.node.name, "gate");
    // The question was out until the step ended its own wait.
    let position = events
        .iter()
        .position(|e| e.id == event.id)
        .expect("in the stream");
    assert_eq!(
        events[position + 1].view(),
        Some(&ViewEvent::WaitStateChanged {
            state: WaitState::Running,
        })
    );
    let replayed = replay_run(dir.path()).expect("projects");
    let from_log = replayed
        .iter()
        .find(|e| e.id == event.id)
        .expect("the expiry is derived from the log");
    let mut live = event.clone();
    live.observed_at = None;
    assert_eq!(*from_log, live, "replay derives the same expiry");
    live
}

/// The gate's answer deadline ends the interviewer's wait, and the record
/// carries the gate's own disposition: timed out, with the default it took,
/// nothing delivered. The public stream says the same, live and on replay,
/// on the gate's firing and attempt.
#[tokio::test]
async fn an_expired_question_is_recorded_as_timed_out_with_the_default_it_took() {
    let dir = RunDir::new("interview-expiry");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, TIMED);
    let token: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let (report, receipt, events) =
        run_projected(&rt, lowered, Arc::new(Silent(token.clone()))).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let ran = ran(&report);
    assert!(ran.contains(&"no") && !ran.contains(&"yes"), "{ran:?}");
    assert!(
        token
            .lock()
            .expect("not poisoned")
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled),
        "the wait was ended when the question expired"
    );
    assert!(
        receipt.is_clean(),
        "an expiry is not an interviewer error: {:?}",
        receipt.errors
    );
    assert_eq!(receipt.questions.len(), 1);
    let record = &receipt.questions[0];
    assert_eq!(record.timeout_ms, Some(300));
    assert_eq!(record.reply, ReplyRecord::TimedOut {
        default: Some("N".to_owned()),
    });
    assert_eq!(record.delivery, Delivery::Expired);
    let expiry = the_expiry(&dir, &events);
    assert_eq!(
        expiry.parsed(),
        Some(&Parsed::QuestionExpired {
            expired: QuestionExpired {
                question:  record.question.clone(),
                waited_ms: 300,
                default:   Some("N".to_owned()),
            },
        })
    );
    let subject = expiry.subject.expect("a firing's event");
    assert_eq!(subject.firing, Some(record.firing));
    assert_eq!(subject.attempt, Some(record.attempt));
}

/// Without a default the expired gate fails with Fabro's retry outcome. The
/// record still says timed out, with no default, and the receipt is clean:
/// the expiry is the gate's disposition, not an interviewer error.
#[tokio::test]
async fn an_expired_question_without_a_default_is_timed_out_and_the_gate_asks_for_a_retry() {
    let dir = RunDir::new("interview-expiry-retry");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, TIMED_NO_DEFAULT);
    let token: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let (report, receipt, events) = run_projected(&rt, lowered, Arc::new(Silent(token))).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(
        failure_class(&report, "gate"),
        Some(FailureClass::new("retry_requested"))
    );
    assert!(!ran(&report).contains(&"yes"));
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    assert_eq!(receipt.questions.len(), 1);
    let record = &receipt.questions[0];
    assert_eq!(record.reply, ReplyRecord::TimedOut { default: None });
    assert_eq!(record.delivery, Delivery::Expired);
    let expiry = the_expiry(&dir, &events);
    assert_eq!(
        expiry.parsed(),
        Some(&Parsed::QuestionExpired {
            expired: QuestionExpired {
                question:  record.question.clone(),
                waited_ms: 300,
                default:   None,
            },
        })
    );
}

/// An interviewer that says to cancel is recorded as cancelled and
/// delivered, apart from a timeout: the gate fails closed with class
/// `interrupted`, and nothing expired.
#[tokio::test]
async fn a_cancelled_reply_is_delivered_and_recorded_apart_from_a_timeout() {
    let dir = RunDir::new("interview-cancelled-reply");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, ONE_GATE);
    let mut answers: BTreeMap<&'static str, Reply> = BTreeMap::new();
    answers.insert("gate", Box::new(|_| InterviewReply::Cancelled));
    let interviewer = Arc::new(ByNode {
        answers,
        seen: Mutex::new(Vec::new()),
        after: None,
    });
    let (report, receipt, events) = run_projected(&rt, lowered, interviewer).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(
        failure_class(&report, "gate"),
        Some(FailureClass::new("interrupted"))
    );
    let ran = ran(&report);
    assert!(!ran.contains(&"yes") && !ran.contains(&"no"), "{ran:?}");
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    assert_eq!(receipt.questions.len(), 1);
    assert_eq!(receipt.questions[0].reply, ReplyRecord::Cancelled);
    assert_eq!(receipt.questions[0].delivery, Delivery::Delivered);
    assert!(expiries(&events).is_empty(), "nothing expired");
    assert!(
        events.iter().any(|event| matches!(
            &event.derived,
            Some(Derived::ControlRequested {
                deliverable: true,
                answer: Some(answer),
            }) if answer.cancelled
        )),
        "the cancellation was delivered as an answer"
    );
}

/// Ignores the cancel token and answers `Y` long after any deadline.
struct LateYes;

#[async_trait::async_trait]
impl Interviewer for LateYes {
    async fn reply(
        &self,
        _request: InterviewRequest,
        _cancel: CancellationToken,
    ) -> InterviewReply {
        sleep(Duration::from_secs(5)).await;
        InterviewReply::Answered(Answer::choice("Y"))
    }
}

/// An answer that would arrive after the deadline never lands: the gate
/// took its default when the question expired, the run did not wait for
/// the interviewer, and the record keeps the timeout.
#[tokio::test]
async fn a_reply_after_the_deadline_never_lands_and_the_record_keeps_the_timeout() {
    let dir = RunDir::new("interview-late-reply");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, TIMED);
    let started = Instant::now();
    let (report, receipt, events) = run_projected(&rt, lowered, Arc::new(LateYes)).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the run did not wait for the late reply"
    );
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let ran = ran(&report);
    assert!(ran.contains(&"no") && !ran.contains(&"yes"), "{ran:?}");
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    assert_eq!(receipt.questions.len(), 1);
    assert_eq!(receipt.questions[0].reply, ReplyRecord::TimedOut {
        default: Some("N".to_owned()),
    });
    assert_eq!(receipt.questions[0].delivery, Delivery::Expired);
    assert_eq!(expiries(&events).len(), 1);
    assert!(
        !events.iter().any(|event| matches!(
            event.derived,
            Some(Derived::ControlRequested {
                answer: Some(_),
                ..
            })
        )),
        "no answer reached the gate"
    );
}
