//! Host tools on the native backend (plan item P1.3): an embedding
//! application registers a `HostTools` capability, and every native agent
//! session gets its tools beside Pebble's. The tools see the stage's
//! Petri-defined context, edit the workspace through Pebble's environment,
//! are blocked by the run's tool hooks like any other tool, reach a
//! sub-agent through Pebble's inheritance when marked for it, and their
//! calls ride the public event stream under the stage's identity.
//!
//! The runs go through the coordinator (`execution::host`), which registers
//! the `ExecutionIdentity` the context is built from; the last test shows a
//! bare driver refusing the node instead of running without the tools.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use attractor_steps::hooks::REPORT_EVENT;
use attractor_steps::host_tools::{HostToolContext, HostTools};
use attractor_steps::pebble::PebbleClient;
use attractor_steps::register;
use execution::events::{CollectingSink, EventProjector, RunEvent};
use execution::host::{self, HostRun};
use execution::{ExecutionId, ExecutionObserver, InvocationId, RunKey};
use frontend::{CompileInputs, Diagnostics, NoFiles};
use frontend_attractor::RunSettings;
use frontend_attractor::hooks::{HookDefinition, HookEvent, HookKind};
use ir::{Attempt, Graph, RunStatus, Status, Value};
use lithos_llm::Client;
use lithos_llm::types::Request;
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedProvider, multi_tool_call_response, routed_client, scripted_client,
    text_response, tool_call_response,
};
use pebble_coding_agent::tools::{RegisteredTool, ToolError, ToolSource};
use runtime::driver::ExecutionReport;
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, backend_event, output_of};

/// The run key the host names its run by.
const RUN_KEY: &str = "host-tools-run";

/// The line the host tool appends when the model calls it.
const NOTE: &str = "hello from the host tool";

fn graph(hooks: Vec<HookDefinition>) -> Graph {
    let source = r#"digraph T {
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        a [prompt="Record what you learn"]
        exit [shape=Msquare]
        start -> a -> exit
    }"#;
    let settings = RunSettings {
        hooks,
        ..RunSettings::default()
    };
    let lowered = frontend_attractor::lower(
        "test.fabro",
        source,
        &NoFiles,
        &CompileInputs::new(),
        settings,
        Diagnostics::new(),
    );
    lowered.graph.unwrap_or_else(|| {
        let diagnostics: Vec<String> = lowered
            .diagnostics
            .iter()
            .map(ToString::to_string)
            .collect();
        panic!("lowers: {diagnostics:?}")
    })
}

/// A command hook as the Fabro frontend resolves one `[[run.hooks]]` entry.
fn command_hook(name: &str, event: HookEvent, script: &str) -> HookDefinition {
    HookDefinition {
        name: name.to_owned(),
        id: None,
        event,
        kind: HookKind::Command {
            command: script.to_owned(),
        },
        matcher: None,
        blocking: None,
        timeout_ms: None,
        sandbox: None,
        source: Some("workflow.toml".to_owned()),
    }
}

/// What the host saw: the context each session was built with, and the
/// context and text of each call the tool answered.
#[derive(Default)]
struct Recorder {
    built: Mutex<Vec<HostToolContext>>,
    calls: Mutex<Vec<(HostToolContext, String)>>,
}

impl Recorder {
    fn built(&self) -> Vec<HostToolContext> {
        self.built
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn calls(&self) -> Vec<(HostToolContext, String)> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The host's two tools: `record_note`, which appends a line to `notes.txt`
/// in the workspace and may reach a sub-agent, and `host_status`, which
/// stays on the root session.
fn host_tools(recorder: &Arc<Recorder>) -> HostTools {
    let recorder = recorder.clone();
    HostTools::new().with(move |context| {
        recorder
            .built
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(context.clone());
        let calls = recorder.clone();
        let context = context.clone();
        let record_note = RegisteredTool::function(
            "record_note",
            "Append a line to notes.txt in the workspace.",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            }),
            move |tool, arguments| {
                let calls = calls.clone();
                let context = context.clone();
                async move {
                    let text = arguments["text"].as_str().unwrap_or_default().to_owned();
                    let existing = tool
                        .env()
                        .read_file_text("notes.txt")
                        .await
                        .unwrap_or_default();
                    tool.env()
                        .write_file("notes.txt", &format!("{existing}{text}\n"))
                        .await
                        .map_err(|error| ToolError::execution(error.to_string()))?;
                    calls
                        .calls
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((context, text));
                    Ok("recorded".to_owned())
                }
            },
        )
        .with_source(ToolSource::Application)
        .allow_in_subagents();
        let host_status = RegisteredTool::function(
            "host_status",
            "The host's status.",
            json!({ "type": "object", "properties": {} }),
            |_, _| async { Ok("ok".to_owned()) },
        )
        .with_source(ToolSource::Application);
        vec![record_note, host_status]
    })
}

fn runtime(dir: &Path, client: Client, tools: HostTools) -> Runtime {
    let mut options = RunOptions::new(dir);
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Always;
    options.echo = false;
    options.run_key = Some(RunKey::new(RUN_KEY));
    register(Runtime::standard())
        .capability(PebbleClient(client))
        .capability(tools)
        .options(options)
}

/// Run `graph` through the coordinator with a public event projector
/// attached; the run's report and every public event.
async fn run(rt: &Runtime, graph: Graph) -> (ExecutionReport, Vec<RunEvent>) {
    let sink = Arc::new(CollectingSink::default());
    let projector = EventProjector::new(sink.clone());
    let report = host::run_configured(
        rt,
        HostRun::new(graph).observe(projector.clone() as Arc<dyn ExecutionObserver>),
        |_, _| {},
    )
    .await
    .expect("the run completes");
    projector.shutdown().await;
    (report, sink.events())
}

/// The root invocation's workspace under the coordinator.
fn workspace(dir: &RunDir) -> PathBuf {
    dir.path().join("scopes/invocation-0-scope-0/work")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// The tool names a request advertised.
fn tool_names(request: &Request) -> Vec<String> {
    request.tools().iter().map(|t| t.name.clone()).collect()
}

/// One `ToolCallCompleted` of a Pebble session on the public stream, with
/// the stage the event was recorded under.
struct Completion {
    node:           String,
    invocation:     Option<InvocationId>,
    execution:      Option<ExecutionId>,
    parent_session: Option<String>,
    payload:        Value,
}

/// Every completed call of `tool` on the public stream.
fn completions(events: &[RunEvent], tool: &str) -> Vec<Completion> {
    events
        .iter()
        .filter_map(|event| {
            let activity = event.custom().and_then(backend_event)?;
            if activity.backend != "pebble" {
                return None;
            }
            let payload = activity.envelope["event"].get("ToolCallCompleted")?;
            if payload["tool_name"] != tool {
                return None;
            }
            Some(Completion {
                node:           event
                    .subject
                    .as_ref()
                    .map(|s| s.node.name.to_string())
                    .unwrap_or_default(),
                invocation:     event.context.invocation,
                execution:      event.context.execution,
                parent_session: activity.parent_session,
                payload:        payload.clone(),
            })
        })
        .collect()
}

/// The `attractor.hook` reports on the public stream for `event`.
fn hook_reports(events: &[RunEvent], event: HookEvent) -> Vec<Value> {
    events
        .iter()
        .filter_map(RunEvent::custom)
        .filter(|value| value["kind"] == REPORT_EVENT && value["event"] == event.as_str())
        .cloned()
        .collect()
}

fn note_call(text: &str) -> ScriptedCall {
    ScriptedCall::response(tool_call_response(
        "record_note",
        "note",
        json!({ "text": text }),
    ))
}

/// The model calls the host's tool; the tool edits the workspace through
/// Pebble's environment; the model reads the result. The host saw one
/// session built with the stage's context, and the same context on the
/// call. The call is on the public stream as the stage's own Pebble event.
#[tokio::test]
async fn a_host_tool_edits_the_workspace_under_the_stages_identity() {
    let dir = RunDir::new("host-tools-edit");
    let recorder = Arc::new(Recorder::default());
    let (client, provider) = scripted_client(vec![
        note_call(NOTE),
        ScriptedCall::response(text_response("Noted.")),
    ]);
    let rt = runtime(dir.path(), client, host_tools(&recorder));
    let (report, events) = run(&rt, graph(Vec::new())).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "Noted.");
    assert_eq!(
        read(&workspace(&dir).join("notes.txt")),
        format!("{NOTE}\n"),
        "the host tool wrote into the stage's workspace"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let advertised = tool_names(&requests[0]);
    assert!(
        advertised.iter().any(|name| name == "record_note")
            && advertised.iter().any(|name| name == "host_status"),
        "the host's tools are advertised beside Pebble's: {advertised:?}"
    );
    assert!(
        advertised.iter().any(|name| name == "shell"),
        "Pebble's own tools stay: {advertised:?}"
    );
    let result = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        result.contains("recorded"),
        "the model read the tool's answer: {result}"
    );

    let built = recorder.built();
    assert_eq!(built.len(), 1, "one session, one build");
    let context = &built[0];
    assert_eq!(context.run, RunKey::new(RUN_KEY));
    assert_eq!(context.invocation, InvocationId::ROOT);
    assert_eq!(context.execution, ExecutionId::new(0));
    assert_eq!(context.node, "a");
    assert_eq!(context.attempt, Attempt::FIRST);
    let calls = recorder.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(&calls[0].0, context, "the call carries the build's context");
    assert_eq!(calls[0].1, NOTE);

    let completed = completions(&events, "record_note");
    assert_eq!(
        completed.len(),
        1,
        "one completed call on the public stream"
    );
    assert_eq!(completed[0].node, "a", "recorded under the stage");
    assert_eq!(completed[0].invocation, Some(InvocationId::ROOT));
    assert_eq!(completed[0].execution, Some(ExecutionId::new(0)));
    assert_eq!(completed[0].payload["is_error"], false);
}

/// A `pre_tool_use` hook blocks the host's tool as it blocks Pebble's: the
/// tool never runs, the model reads the reason, the report is on the
/// stream, and Pebble's completion says the call was denied.
#[tokio::test]
async fn a_hook_blocks_a_host_tool() {
    let dir = RunDir::new("host-tools-hook");
    let recorder = Arc::new(Recorder::default());
    let (client, provider) = scripted_client(vec![
        note_call(NOTE),
        ScriptedCall::response(text_response("The note was refused.")),
    ]);
    let hooks = vec![command_hook(
        "no-notes",
        HookEvent::PreToolUse,
        r#"if grep -q 'record_note' "$FABRO_HOOK_CONTEXT"; then echo '{"decision":"block","reason":"notes are not allowed here"}'; exit 2; fi"#,
    )];
    let rt = runtime(dir.path(), client, host_tools(&recorder));
    let (report, events) = run(&rt, graph(hooks)).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(
        !workspace(&dir).join("notes.txt").exists(),
        "the blocked tool never wrote"
    );
    assert_eq!(recorder.built().len(), 1);
    assert!(recorder.calls().is_empty(), "the tool's executor never ran");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let denial = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        denial.contains("notes are not allowed here"),
        "the model saw the block reason: {denial}"
    );

    let reports = hook_reports(&events, HookEvent::PreToolUse);
    assert_eq!(reports.len(), 1, "one pre_tool_use report: {reports:?}");
    assert_eq!(reports[0]["node"], "a");
    let report_text = serde_json::to_string(&reports[0]["report"]).expect("report");
    assert!(
        report_text.contains("notes are not allowed here"),
        "the report carries the block: {report_text}"
    );
    let completed = completions(&events, "record_note");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].node, "a");
    assert_eq!(completed[0].payload["is_error"], true);
    assert_eq!(completed[0].payload["error_kind"], "denied");
}

/// A tool marked `allow_in_subagents` reaches a child through Pebble's own
/// inheritance; one that is not stays on the root. The child's call runs
/// with the parent stage's context, edits the parent's workspace, and is
/// recorded under the parent stage naming the parent's session.
#[tokio::test]
async fn a_sub_agent_calls_an_inherited_host_tool() {
    let dir = RunDir::new("host-tools-subagent");
    let recorder = Arc::new(Recorder::default());
    let task = "child: record the note";
    let (client, provider) = routed_client(
        ScriptedProvider::new(vec![
            ScriptedCall::response(multi_tool_call_response(vec![(
                "spawn_agent",
                "spawn-0",
                json!({ "task": task }),
            )])),
            ScriptedCall::response(tool_call_response("wait", "wait", json!({}))),
            ScriptedCall::response(text_response("The child recorded it.")),
        ]),
        vec![(
            task,
            ScriptedProvider::new(vec![
                note_call("from the child"),
                ScriptedCall::response(text_response("Recorded.")),
            ]),
        )],
    );
    let rt = runtime(dir.path(), client, host_tools(&recorder));
    let (report, events) = run(&rt, graph(Vec::new())).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "The child recorded it.");
    assert_eq!(
        read(&workspace(&dir).join("notes.txt")),
        "from the child\n",
        "the child's call edited the parent's workspace"
    );

    let root = tool_names(&provider.root().requests()[0]);
    assert!(
        root.iter().any(|name| name == "record_note")
            && root.iter().any(|name| name == "host_status"),
        "the root has both host tools: {root:?}"
    );
    let child_requests = provider.lane(task).requests();
    assert_eq!(child_requests.len(), 2);
    let child = tool_names(&child_requests[0]);
    assert!(
        child.iter().any(|name| name == "record_note"),
        "the child inherited the marked tool: {child:?}"
    );
    assert!(
        !child.iter().any(|name| name == "host_status"),
        "the child did not get the root-only tool: {child:?}"
    );

    let built = recorder.built();
    assert_eq!(built.len(), 1, "the child inherits; nothing builds again");
    let calls = recorder.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0, built[0],
        "the child's call carries the parent stage's context"
    );
    assert_eq!(calls[0].0.node, "a");
    assert_eq!(calls[0].1, "from the child");

    let completed = completions(&events, "record_note");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].node, "a", "recorded under the parent stage");
    assert!(
        completed[0].parent_session.is_some(),
        "the child's event names its parent session"
    );
    assert_eq!(completed[0].payload["is_error"], false);
}

/// A driver built outside the coordinator registers no `ExecutionIdentity`,
/// so a run that asked for host tools fails its agent node routably rather
/// than run without them.
#[tokio::test]
async fn a_bare_driver_with_host_tools_fails_the_node_routably() {
    let dir = RunDir::new("host-tools-bare");
    let recorder = Arc::new(Recorder::default());
    let (client, provider) = scripted_client(vec![ScriptedCall::response(text_response("Never."))]);
    let report = runtime(dir.path(), client, host_tools(&recorder))
        .run(graph(Vec::new()))
        .await
        .expect("replay");
    let row = report
        .state
        .history()
        .iter()
        .find(|row| row.name == "a")
        .expect("the agent node ran");
    match &row.outcome.status {
        Status::Failure(info) => {
            assert_eq!(info.class.as_str(), "capability_unavailable");
            assert!(
                info.message.contains("ExecutionIdentity"),
                "the message names what is missing: {}",
                info.message
            );
        }
        other => panic!(
            "expected a failure, got {other:?} (run {:?}, history {:?})",
            report.status,
            report
                .state
                .history()
                .iter()
                .map(|row| (row.name.to_string(), row.outcome.status.clone()))
                .collect::<Vec<_>>()
        ),
    }
    assert!(recorder.built().is_empty(), "no session was built");
    assert!(provider.requests().is_empty(), "no model call was made");
}
