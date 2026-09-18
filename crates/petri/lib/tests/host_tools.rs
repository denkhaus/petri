//! The host tool registry through the embedding boundary (plan item P1.3):
//! a host on the `petri` distribution installs `HostTools` beside
//! `PebbleClient`, runs a workflow through `execution::host`, and reads the
//! tool's call back from the public event stream alone, under the stage's
//! identity, with the tool's effect in the workspace. Real Pebble against a
//! scripted model; no Fabro.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pebble_coding_agent::test_support::{
    ScriptedCall, scripted_client, text_response, tool_call_response,
};
use pebble_coding_agent::tools::{RegisteredTool, ToolError, ToolSource};
use petri::attractor::host_tools::{HostToolContext, HostTools};
use petri::attractor::pebble::PebbleClient;
use petri::attractor::register;
use petri::execution::events::{CollectingSink, EventProjector, RunEvent};
use petri::execution::host::{self, HostRun};
use petri::execution::{ExecutionId, ExecutionObserver, InvocationId, RunKey};
use petri::executor::Retention;
use petri::frontend::{CompileInputs, NoFiles, attractor};
use petri::ir::{Attempt, Graph, RunStatus};
use petri::{RunOptions, Runtime};
use serde_json::{Value, json};
use testkit::{RunDir, backend_event, output_of};

const RUN_KEY: &str = "embedded-host-tools";

fn graph() -> Graph {
    let source = r#"digraph T {
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        a [prompt="Tell the host what you found"]
        exit [shape=Msquare]
        start -> a -> exit
    }"#;
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

/// The host's tool: `report_finding` writes the finding into the workspace
/// and remembers the context of each call.
fn host_tools(calls: &Arc<Mutex<Vec<(HostToolContext, String)>>>) -> HostTools {
    let calls = calls.clone();
    HostTools::new().with(move |context| {
        let calls = calls.clone();
        let context = context.clone();
        vec![
            RegisteredTool::function(
                "report_finding",
                "Report a finding to the host.",
                json!({
                    "type": "object",
                    "properties": { "finding": { "type": "string" } },
                    "required": ["finding"],
                }),
                move |tool, arguments| {
                    let calls = calls.clone();
                    let context = context.clone();
                    async move {
                        let finding = arguments["finding"].as_str().unwrap_or_default().to_owned();
                        tool.env()
                            .write_file("finding.txt", &finding)
                            .await
                            .map_err(|error| ToolError::execution(error.to_string()))?;
                        calls
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push((context, finding));
                        Ok("the host has it".to_owned())
                    }
                },
            )
            .with_source(ToolSource::Application),
        ]
    })
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// The stage each completed `report_finding` call was recorded under:
/// `(node, invocation, execution, payload)`.
fn completions(
    events: &[RunEvent],
) -> Vec<(String, Option<InvocationId>, Option<ExecutionId>, Value)> {
    events
        .iter()
        .filter_map(|event| {
            let activity = event.custom().and_then(backend_event)?;
            let payload = activity.envelope["event"].get("ToolCallCompleted")?;
            (activity.backend == "pebble" && payload["tool_name"] == "report_finding").then(|| {
                (
                    event
                        .subject
                        .as_ref()
                        .map(|s| s.node.name.to_string())
                        .unwrap_or_default(),
                    event.context.invocation,
                    event.context.execution,
                    payload.clone(),
                )
            })
        })
        .collect()
}

#[tokio::test]
async fn a_host_tool_installed_through_the_embedding_boundary_is_called_and_observed() {
    let dir = RunDir::new("embedding-host-tools");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "report_finding",
            "finding",
            json!({ "finding": "the build is green" }),
        )),
        ScriptedCall::response(text_response("Reported.")),
    ]);
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Always;
    options.echo = false;
    options.run_key = Some(RunKey::new(RUN_KEY));
    let rt = register(Runtime::standard())
        .capability(PebbleClient(client))
        .capability(host_tools(&calls))
        .options(options);

    let sink = Arc::new(CollectingSink::default());
    let projector = EventProjector::new(sink.clone());
    let report = host::run_configured(
        &rt,
        HostRun::new(graph()).observe(projector.clone() as Arc<dyn ExecutionObserver>),
        |_, _| {},
    )
    .await
    .expect("the run completes");
    projector.shutdown().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "Reported.");
    assert_eq!(
        read(
            &dir.path()
                .join("scopes/invocation-0-scope-0/work/finding.txt")
        ),
        "the build is green",
        "the host tool's effect is in the stage's workspace"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .tools()
            .iter()
            .any(|t| t.name == "report_finding"),
        "the host's tool was advertised"
    );
    assert!(
        serde_json::to_string(&requests[1])
            .expect("request")
            .contains("the host has it"),
        "the model read the host's answer"
    );

    let calls = calls.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(calls.len(), 1);
    let (context, finding) = &calls[0];
    assert_eq!(finding, "the build is green");
    assert_eq!(context.run, RunKey::new(RUN_KEY));
    assert_eq!(context.invocation, InvocationId::ROOT);
    assert_eq!(context.execution, ExecutionId::new(0));
    assert_eq!(context.node, "a");
    assert_eq!(context.attempt, Attempt::FIRST);

    let events = sink.events();
    let completed = completions(&events);
    assert_eq!(
        completed.len(),
        1,
        "one completed call on the public stream"
    );
    let (node, invocation, execution, payload) = &completed[0];
    assert_eq!(node, "a", "the call is the stage's own agent activity");
    assert_eq!(*invocation, Some(context.invocation));
    assert_eq!(*execution, Some(context.execution));
    assert_eq!(payload["is_error"], false);
}
