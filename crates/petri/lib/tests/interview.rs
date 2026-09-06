//! The interview dispatcher against real Fabro human gates on the standalone
//! host: correlation, out-of-order answers from parallel gates, sensitive
//! masking, a failing interviewer, cancellation, and the receipt.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use petri::execution::host::{self, HostRun};
use petri::execution::{
    Delivery, InterviewDispatcher, InterviewError, InterviewReceipt, InterviewReply,
    InterviewRequest, Interviewer, ReplyRecord,
};
use petri::executor::Retention;
use petri::frontend::CompileInputs;
use petri::ir::RunStatus;
use petri::steps::Answer;
use petri::{RunOptions, Runtime, driver};
use serde_json::json;
use testkit::RunDir;
use tokio_util::sync::CancellationToken;

fn runtime(dir: &RunDir) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    petri::runtime().options(options)
}

fn lower(rt: &Runtime, dir: &RunDir, text: &str) -> petri::frontend::Lowered {
    let path = dir.path().join("wf.fabro");
    std::fs::write(&path, text).expect("write the workflow");
    rt.check(&path, None, None, &CompileInputs::new())
        .expect("loads")
}

/// A test interviewer: answers by node name, in the order the closures say.
struct ByNode {
    answers: BTreeMap<&'static str, Box<dyn Fn(&InterviewRequest) -> InterviewReply + Send + Sync>>,
    seen:    Mutex<Vec<InterviewRequest>>,
    /// Hold the reply to this node until the other node was seen.
    after:   Option<(&'static str, &'static str)>,
}

#[async_trait::async_trait]
impl Interviewer for ByNode {
    async fn reply(&self, request: InterviewRequest, cancel: CancellationToken) -> InterviewReply {
        self.seen.lock().expect("not poisoned").push(request.clone());
        if let Some((held, until)) = self.after
            && request.node == held
        {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
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
                    () = tokio::time::sleep(Duration::from_millis(5)) => {},
                    () = cancel.cancelled() => return InterviewReply::Cancelled,
                }
                assert!(tokio::time::Instant::now() < deadline, "`{until}` never asked");
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
    lowered: petri::frontend::Lowered,
    interviewer: Arc<dyn Interviewer>,
) -> (driver::ExecutionReport, InterviewReceipt) {
    let dispatcher = InterviewDispatcher::new(interviewer);
    let host_run = HostRun::new(lowered.graph.expect("lowers"))
        .with_children(lowered.children)
        .observe(Arc::new(dispatcher.clone()));
    let report = host::run_configured(rt, host_run, |handle, secrets| {
        dispatcher.wire(handle, secrets);
    })
    .await
    .expect("the run completes");
    let receipt = dispatcher.shutdown().await;
    (report, receipt)
}

const TWO_GATES: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    fan [shape=component]
    a [shape=hexagon, label="A?", question_type="yes_no"]
    b [shape=hexagon, label="B?", question_type="yes_no"]
    a_yes [shape=parallelogram, script="echo a-yes"]
    a_no [shape=parallelogram, script="echo a-no"]
    b_yes [shape=parallelogram, script="echo b-yes"]
    b_no [shape=parallelogram, script="echo b-no"]
    a_done [shape=parallelogram, script="echo a-done"]
    b_done [shape=parallelogram, script="echo b-done"]
    join [shape=tripleoctagon]
    start -> fan
    fan -> a
    fan -> b
    a -> a_yes [label="[Y] Yes"]
    a -> a_no [label="[N] No"]
    b -> b_yes [label="[Y] Yes"]
    b -> b_no [label="[N] No"]
    a_yes -> a_done
    a_no -> a_done
    b_yes -> b_done
    b_no -> b_done
    a_done -> join
    b_done -> join
    join -> exit
}"#;

#[tokio::test]
async fn parallel_gates_are_answered_out_of_order_and_each_answer_lands_on_its_own_firing() {
    let dir = RunDir::new("interview-parallel");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, TWO_GATES);
    let mut answers: BTreeMap<
        &'static str,
        Box<dyn Fn(&InterviewRequest) -> InterviewReply + Send + Sync>,
    > = BTreeMap::new();
    answers.insert("a", Box::new(|_| InterviewReply::Answered(Answer::choice("N"))));
    answers.insert("b", Box::new(|_| InterviewReply::Answered(Answer::choice("Y"))));
    let interviewer = Arc::new(ByNode {
        answers,
        seen: Mutex::new(Vec::new()),
        // `a` is asked first but answered after `b`.
        after: Some(("a", "b")),
    });
    let (report, receipt) = run(&rt, lowered, interviewer.clone()).await;
    assert_eq!(report.status, RunStatus::Success, "{:?}", report.state.errors());
    let ran: Vec<_> = report
        .state
        .history()
        .iter()
        .map(|record| record.name.as_str())
        .collect();
    assert!(ran.contains(&"a_no"), "{ran:?}");
    assert!(ran.contains(&"b_yes"), "{ran:?}");
    assert!(!ran.contains(&"a_yes"), "{ran:?}");
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    assert_eq!(receipt.questions.len(), 2);
    for record in &receipt.questions {
        assert_eq!(record.delivery, Delivery::Delivered);
        assert_eq!(record.invocation_path, "/");
        assert_eq!(record.occurrence, 1);
        assert_eq!(record.ask, 1);
        assert_eq!(record.kind.as_deref(), Some("yes_no"));
    }
    let seen = interviewer.seen.lock().expect("not poisoned").clone();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].firing, seen[1].firing);
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
    let mut answers: BTreeMap<
        &'static str,
        Box<dyn Fn(&InterviewRequest) -> InterviewReply + Send + Sync>,
    > = BTreeMap::new();
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
    assert_eq!(report.status, RunStatus::Success, "{:?}", report.state.errors());
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    let record = &receipt.questions[0];
    assert!(record.sensitive);
    let ReplyRecord::Answered { text, .. } = &record.reply else {
        panic!("answered: {:?}", record.reply);
    };
    assert_eq!(*text, Some(json!({ "$secret": format!("answer:{}", record.question) })));
    let rendered = serde_json::to_string(&receipt).expect("encodes");
    assert!(!rendered.contains("hunter2"), "{rendered}");
    let log = serde_json::to_string(&report.state.log).expect("encodes");
    assert!(!log.contains("hunter2"), "the plaintext never enters the log");
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
    assert!(matches!(receipt.questions[0].reply, ReplyRecord::Failed { .. }));
    assert_eq!(receipt.questions[0].delivery, Delivery::Delivered);
    let ran: Vec<_> = report
        .state
        .history()
        .iter()
        .map(|record| record.name.as_str())
        .collect();
    assert!(!ran.contains(&"yes") && !ran.contains(&"no"), "{ran:?}");
}

#[tokio::test]
async fn a_cancelled_run_ends_the_pending_wait_and_shutdown_leaves_no_task_behind() {
    let dir = RunDir::new("interview-cancel");
    let rt = runtime(&dir);
    let lowered = lower(&rt, &dir, ONE_GATE);
    /// Never answers; keeps the cancel token so the test can see it fire.
    struct Silent(Arc<Mutex<Option<CancellationToken>>>);
    #[async_trait::async_trait]
    impl Interviewer for Silent {
        async fn reply(
            &self,
            _request: InterviewRequest,
            cancel: CancellationToken,
        ) -> InterviewReply {
            *self.0.lock().expect("not poisoned") = Some(cancel.clone());
            cancel.cancelled().await;
            InterviewReply::Cancelled
        }
    }
    let token = Arc::new(Mutex::new(None));
    let dispatcher = InterviewDispatcher::new(Arc::new(Silent(token.clone())));
    let host_run = HostRun::new(lowered.graph.expect("lowers"))
        .with_children(lowered.children)
        .observe(Arc::new(dispatcher.clone()));
    let mut cancel = None;
    let report = host::run_configured(&rt, host_run, |handle, secrets| {
        dispatcher.wire(handle.clone(), secrets);
        cancel = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            handle.cancel_root();
        }));
    })
    .await
    .expect("the run completes");
    if let Some(cancel) = cancel {
        let _ = cancel.await;
    }
    let receipt = tokio::time::timeout(Duration::from_secs(10), dispatcher.shutdown())
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
