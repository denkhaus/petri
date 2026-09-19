//! The interrupt control on the standalone host: a host stops a live agent
//! stage's current model turn, the session survives, and the stage continues
//! with its next input. Real Pebble against a scripted model with a host tool
//! that runs until it is cancelled; the ACP backend against the fake agent
//! Fabro ships; a human gate for the stage that has no turn to stop.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pebble_coding_agent::test_support::{
    ScriptedCall, scripted_client, text_response, tool_call_response,
};
use pebble_coding_agent::tools::{RegisteredTool, ToolError, ToolSource};
use petri::attractor::agent::INTERRUPTED_EVENT;
use petri::attractor::host_tools::HostTools;
use petri::attractor::pebble::PebbleClient;
use petri::attractor::register;
use petri::engine::{EngineState, Event, EventRecord};
use petri::execution::controls::{ControlError, ControlService};
use petri::execution::host::{self, HostRun};
use petri::execution::{
    CoordinatorRecord, ExecutionId, ExecutionObserver, InterviewDispatcher, InterviewReply,
    InterviewRequest, Interviewer,
};
use petri::executor::Retention;
use petri::frontend::{CompileInputs, NoFiles, attractor};
use petri::ir::{Control, ExprOrValue, Graph, RunStatus, StepEvent, Value};
use petri::steps::{Answer, INTERRUPT_KEY};
use petri::{RunOptions, Runtime, driver};
use serde_json::json;
use testkit::{RunDir, output_of, wait_for_file};
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(20);

fn lower(source: &str) -> Graph {
    let lowered = attractor::load("test.fabro", source, &NoFiles, &CompileInputs::new());
    lowered.graph.unwrap_or_else(|| {
        let diagnostics: Vec<String> = lowered
            .diagnostics
            .iter()
            .map(ToString::to_string)
            .collect();
        panic!("lowers: {diagnostics:?}")
    })
}

fn native_graph() -> Graph {
    lower(
        r#"digraph T {
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        a [prompt="Wait for the host, then report"]
        exit [shape=Msquare]
        start -> a -> exit
    }"#,
    )
}

fn options(dir: &RunDir) -> RunOptions {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    options
}

/// The runtime with the control service installed the way a host installs
/// it: its pause hooks and its live-turn set.
fn controlled(rt: Runtime, controls: &ControlService) -> Runtime {
    rt.hooks(controls.hooks(None)).capability(controls.turns())
}

/// The host's tool: `wait_for_the_host` says it was entered, then runs until
/// the call is cancelled. A model turn that calls it stays in flight until
/// something stops it.
fn waiting_tool(entered: &Arc<Notify>) -> HostTools {
    let entered = entered.clone();
    HostTools::new().with(move |_context| {
        let entered = entered.clone();
        vec![
            RegisteredTool::function(
                "wait_for_the_host",
                "Wait until the host says to stop.",
                json!({ "type": "object", "properties": {} }),
                move |tool, _arguments| {
                    let entered = entered.clone();
                    async move {
                        entered.notify_one();
                        tool.cancel().cancelled().await;
                        Err(ToolError::execution("the host stopped the call"))
                    }
                },
            )
            .with_source(ToolSource::Application),
        ]
    })
}

/// Notifies once per `step.progress.recorded` custom payload of `kind`.
struct OnCustom {
    kind: &'static str,
    seen: Arc<Notify>,
}

impl ExecutionObserver for OnCustom {
    fn on_engine_record(
        &self,
        _: ExecutionId,
        record: &EventRecord,
        _recorded_at: u64,
        _: &EngineState,
    ) {
        if let Event::StepProgressRecorded {
            ev: StepEvent::Custom(value),
            ..
        } = &record.event
            && value["kind"] == self.kind
        {
            self.seen.notify_one();
        }
    }

    fn on_lifecycle(&self, _: &CoordinatorRecord) {}
}

/// Run `graph` with `controls` observed and wired, plus `observers`.
async fn run_controlled(
    rt: &Runtime,
    graph: Graph,
    controls: &ControlService,
    observers: Vec<Arc<dyn ExecutionObserver>>,
) -> driver::ExecutionReport {
    let mut host_run = HostRun::new(graph).observe(Arc::new(controls.clone()));
    for observer in observers {
        host_run = host_run.observe(observer);
    }
    host::run_configured(rt, host_run, |handle, _| controls.wire(handle))
        .await
        .expect("the run completes")
}

/// The `step.progress.recorded` custom payloads of the run, in order.
fn customs(report: &driver::ExecutionReport) -> Vec<Value> {
    report
        .state
        .log
        .events()
        .filter_map(|event| match event {
            Event::StepProgressRecorded {
                ev: StepEvent::Custom(value),
                ..
            } => Some(value.clone()),
            _ => None,
        })
        .collect()
}

/// The delivered values of the run's `control.requested` records.
fn deliveries(report: &driver::ExecutionReport) -> Vec<Value> {
    report
        .state
        .log
        .events()
        .filter_map(|event| match event {
            Event::ControlRequested {
                ctl: Control::Deliver(value),
                ..
            } => Some(value.clone()),
            _ => None,
        })
        .collect()
}

fn interrupted_events(customs: &[Value]) -> Vec<&Value> {
    customs
        .iter()
        .filter(|value| value["kind"] == INTERRUPTED_EVENT)
        .collect()
}

/// Every Pebble envelope's session, and whether one round was interrupted.
fn pebble_sessions(customs: &[Value]) -> (Vec<String>, usize) {
    let mut sessions = Vec::new();
    let mut interrupted = 0;
    for value in customs.iter().filter(|value| value["kind"] == "pebble") {
        if let Some(session) = value["event"]["session_id"].as_str() {
            sessions.push(session.to_owned());
        }
        if value["event"]["event"].get("RoundInterrupted").is_some() {
            interrupted += 1;
        }
    }
    sessions.sort();
    sessions.dedup();
    (sessions, interrupted)
}

/// An interrupt with text, during a tool call: the call is cancelled, the
/// turn ends without an answer, the session stays open, and the text opens
/// the next turn, whose answer is the stage's.
#[tokio::test]
async fn an_interrupt_with_text_ends_the_turn_and_the_text_opens_the_next() {
    let dir = RunDir::new("interrupt-with-text");
    let entered = Arc::new(Notify::new());
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response("wait_for_the_host", "call-1", json!({}))),
        ScriptedCall::response(text_response("steered answer")),
    ]);
    let controls = ControlService::new();
    let rt = controlled(
        register(Runtime::standard())
            .capability(PebbleClient(client))
            .capability(waiting_tool(&entered))
            .options(options(&dir)),
        &controls,
    );
    let interrupter = {
        let controls = controls.clone();
        let entered = entered.clone();
        tokio::spawn(async move {
            timeout(WAIT, entered.notified())
                .await
                .expect("the tool call started");
            controls
                .interrupt_and_steer("a", "stop and summarize what you have")
                .await
        })
    };
    let report = run_controlled(&rt, native_graph(), &controls, Vec::new()).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        timeout(WAIT, interrupter)
            .await
            .expect("interrupted")
            .expect("task"),
        Ok(())
    );
    assert_eq!(output_of(&report, "a")["text"], "steered answer");

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "the interrupted turn, then the steered one"
    );
    let second = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        second.contains("stop and summarize what you have"),
        "the text reached the model as its next input"
    );

    let delivered = deliveries(&report);
    assert_eq!(delivered.len(), 1);
    assert_eq!(
        delivered[0][INTERRUPT_KEY]["steer"], "stop and summarize what you have",
        "the record carries the interrupt and its text: {delivered:?}"
    );

    let customs = customs(&report);
    let (sessions, rounds_interrupted) = pebble_sessions(&customs);
    assert_eq!(sessions.len(), 1, "one session, kept across the interrupt");
    assert_eq!(rounds_interrupted, 1, "Pebble reported the stopped round");
    let interrupted = interrupted_events(&customs);
    assert_eq!(interrupted.len(), 1, "{customs:?}");
    assert_eq!(interrupted[0]["node"], "a");
    assert_eq!(interrupted[0]["backend"], "api");
    assert_eq!(interrupted[0]["session"], sessions[0]);
}

/// A plain interrupt parks the turn; the next steer is its next input.
#[tokio::test]
async fn a_plain_interrupt_waits_for_the_next_steer() {
    let dir = RunDir::new("interrupt-then-steer");
    let entered = Arc::new(Notify::new());
    let interrupted = Arc::new(Notify::new());
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response("wait_for_the_host", "call-1", json!({}))),
        ScriptedCall::response(text_response("resumed answer")),
    ]);
    let controls = ControlService::new();
    let rt = controlled(
        register(Runtime::standard())
            .capability(PebbleClient(client))
            .capability(waiting_tool(&entered))
            .options(options(&dir)),
        &controls,
    );
    let driver_task = {
        let controls = controls.clone();
        let entered = entered.clone();
        let interrupted = interrupted.clone();
        tokio::spawn(async move {
            timeout(WAIT, entered.notified())
                .await
                .expect("the tool call started");
            let first = controls.interrupt("a").await;
            timeout(WAIT, interrupted.notified())
                .await
                .expect("the stage reported the interrupt");
            let then = controls.steer("a", "continue with a summary").await;
            (first, then)
        })
    };
    let watcher: Arc<dyn ExecutionObserver> = Arc::new(OnCustom {
        kind: INTERRUPTED_EVENT,
        seen: interrupted.clone(),
    });
    let report = run_controlled(&rt, native_graph(), &controls, vec![watcher]).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        timeout(WAIT, driver_task)
            .await
            .expect("driven")
            .expect("task"),
        (Ok(()), Ok(()))
    );
    assert_eq!(output_of(&report, "a")["text"], "resumed answer");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        serde_json::to_string(&requests[1])
            .expect("request")
            .contains("continue with a summary"),
        "the steer woke the parked prompt as its next input"
    );
    let delivered = deliveries(&report);
    assert_eq!(delivered.len(), 2, "the interrupt, then the steer");
    assert_eq!(delivered[0], json!({ INTERRUPT_KEY: {} }));
    assert_eq!(delivered[1]["$steer"]["text"], "continue with a summary");
    let customs = customs(&report);
    let (sessions, rounds_interrupted) = pebble_sessions(&customs);
    assert_eq!(sessions.len(), 1);
    assert_eq!(rounds_interrupted, 1);
    assert_eq!(interrupted_events(&customs).len(), 1);
}

/// A stage that is live but has no model turn (a human gate waiting on its
/// question) refuses the interrupt by name; so does a stage that is not
/// running.
#[tokio::test]
async fn an_interrupt_on_a_stage_with_no_live_turn_is_refused() {
    const GATE: &str = r#"digraph G {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        gate [shape=hexagon, label="Go?"]
        go [shape=parallelogram, script="echo go"]
        start -> gate
        gate -> go [label="[Y] Yes"]
        go -> exit
    }"#;
    struct InterruptTheGate {
        controls: ControlService,
        results:  Mutex<Vec<Result<(), ControlError>>>,
    }
    #[async_trait::async_trait]
    impl Interviewer for InterruptTheGate {
        async fn reply(&self, request: InterviewRequest, _: CancellationToken) -> InterviewReply {
            let on_gate = self.controls.interrupt(&request.node).await;
            let on_missing = self.controls.interrupt("missing").await;
            let with_text = self.controls.interrupt_and_steer(&request.node, "go").await;
            self.results
                .lock()
                .expect("not poisoned")
                .extend([on_gate, on_missing, with_text]);
            InterviewReply::Answered(Answer::choice("Y"))
        }
    }
    let dir = RunDir::new("interrupt-no-turn");
    let controls = ControlService::new();
    let rt = controlled(
        register(Runtime::standard()).options(options(&dir)),
        &controls,
    );
    let interviewer = Arc::new(InterruptTheGate {
        controls: controls.clone(),
        results:  Mutex::new(Vec::new()),
    });
    let dispatcher = InterviewDispatcher::new(interviewer.clone());
    let host_run = HostRun::new(lower(GATE))
        .observe(Arc::new(controls.clone()))
        .observe(Arc::new(dispatcher.clone()));
    let report = host::run_configured(&rt, host_run, |handle, secrets| {
        dispatcher.wire(handle.clone(), secrets);
        controls.wire(handle);
    })
    .await
    .expect("the run completes");
    let receipt = dispatcher.shutdown().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(receipt.is_clean(), "{:?}", receipt.errors);
    assert_eq!(*interviewer.results.lock().expect("not poisoned"), vec![
        Err(ControlError::NoLiveTurn),
        Err(ControlError::NoSuchStage("missing".into())),
        Err(ControlError::NoLiveTurn),
    ]);
    assert_eq!(
        deliveries(&report).len(),
        1,
        "only the answer was delivered; a refused interrupt is not recorded"
    );
    assert!(interrupted_events(&customs(&report)).is_empty());
}

/// The fake agent, copied from the packaged test data to where a run can
/// execute it.
fn fake_agent(dir: &RunDir) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fabro/acceptance/testdata/fake_acp_agent.py");
    let script =
        fs::read_to_string(&source).unwrap_or_else(|e| panic!("{}: {e}", source.display()));
    let path = dir.path().join("fake_acp_agent.py");
    fs::write(&path, script).expect("write the fake agent");
    path
}

/// The fake agent's behaviour is chosen through `ACP_MODE` and friends in
/// the environment: set them on the scope so the agent process sees them.
fn with_env(mut graph: Graph, pairs: &[(&str, &str)]) -> Graph {
    for scope in &mut graph.body.scopes {
        for (key, value) in pairs {
            scope
                .env
                .insert((*key).into(), ExprOrValue::Value(json!(value)));
        }
    }
    graph
}

/// On the ACP backend an interrupt is `session/cancel` without ending the
/// process: the agent answers the prompt in flight with `cancelled`, the
/// session continues, and the text is its next prompt.
#[tokio::test]
async fn an_acp_interrupt_cancels_the_turn_and_the_text_is_the_next_prompt() {
    let dir = RunDir::new("interrupt-acp");
    let agent = fake_agent(&dir);
    let first_prompt = dir.path().join("first-prompt.json");
    let cancel_record = dir.path().join("cancel.txt");
    let steered_prompt = dir.path().join("steered-prompt.json");
    let graph = lower(&format!(
        r#"digraph T {{
        graph [goal="Greet", backend="acp", acp.command="python3 {}"]
        start [shape=Mdiamond]
        a [prompt="Say hello"]
        exit [shape=Msquare]
        start -> a -> exit
    }}"#,
        agent.display()
    ));
    let graph = with_env(graph, &[
        ("ACP_MODE", "interrupt_steer"),
        ("ACP_PROMPT_RECORD", first_prompt.to_str().expect("utf-8")),
        ("ACP_CANCEL_RECORD", cancel_record.to_str().expect("utf-8")),
        (
            "ACP_STEER_PROMPT_RECORD",
            steered_prompt.to_str().expect("utf-8"),
        ),
    ]);
    let controls = ControlService::new();
    let rt = controlled(
        register(Runtime::standard()).options(options(&dir)),
        &controls,
    );
    let interrupter = {
        let controls = controls.clone();
        let first_prompt = first_prompt.clone();
        tokio::spawn(async move {
            assert!(
                wait_for_file(&first_prompt, WAIT).await,
                "the agent received the first prompt"
            );
            controls
                .interrupt_and_steer("a", "stop and summarize")
                .await
        })
    };
    let report = run_controlled(&rt, graph, &controls, Vec::new()).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        timeout(WAIT, interrupter)
            .await
            .expect("interrupted")
            .expect("task"),
        Ok(())
    );
    assert_eq!(
        fs::read_to_string(&cancel_record)
            .ok()
            .as_deref()
            .map(str::trim),
        Some("session/cancel"),
        "the agent saw session/cancel"
    );
    assert_eq!(
        output_of(&report, "a")["text"],
        "steered:stop and summarize",
        "the interrupted turn's partial text is not the answer; the next prompt's is"
    );
    let steered: Value = serde_json::from_str(
        &fs::read_to_string(&steered_prompt).expect("the agent recorded the next prompt"),
    )
    .expect("json");
    assert_eq!(steered["prompt"][0]["text"], "stop and summarize");
    let delivered = deliveries(&report);
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0][INTERRUPT_KEY]["steer"], "stop and summarize");
    let customs = customs(&report);
    let interrupted = interrupted_events(&customs);
    assert_eq!(interrupted.len(), 1, "{customs:?}");
    assert_eq!(interrupted[0]["node"], "a");
    assert_eq!(interrupted[0]["backend"], "acp");
    assert_eq!(interrupted[0]["session"], "sess-1");
}
