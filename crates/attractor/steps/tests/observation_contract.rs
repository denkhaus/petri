//! The observation presence contract (fabro fork, ADR-0038): an agent
//! node that declares `x.observations="required"` must call the host's
//! observation tool at least once before its stage can succeed. The
//! engine counts completed, non-error calls — an errored call recorded
//! nothing — and fails the stage with the output contract's class, so
//! failure promotion applies as it does to an exhausted repair.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use attractor_steps::agent::OBSERVATION_TOOL_NAME;
use attractor_steps::host_tools::HostTools;
use attractor_steps::pebble::PebbleClient;
use attractor_steps::register;
use execution::host::{self, HostRun};
use execution::{ExecutionObserver, RunKey};
use frontend::{CompileInputs, Diagnostics, NoFiles};
use frontend_attractor::RunSettings;
use ir::Graph;
use lithos_llm::Client;
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedProvider, scripted_client, text_response, tool_call_response,
};
use pebble_coding_agent::tools::{RegisteredTool, ToolSource};
use runtime::driver::ExecutionReport;
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, output_of};

/// The run key the host names its run by.
const RUN_KEY: &str = "observation-contract-run";

/// A graph whose agent node `a` declares the contract.
fn graph(required: bool) -> Graph {
    let attribute = if required {
        r#", x.observations="required""#
    } else {
        ""
    };
    let source = format!(
        r#"digraph T {{
            graph [backend="api", default_model="test/model"]
            start [shape=Mdiamond]
            a [prompt="Record what you learn"{attribute}]
            exit [shape=Msquare]
            start -> a -> exit
        }}"#
    );
    let lowered = frontend_attractor::lower(
        "test.fabro",
        &source,
        &NoFiles,
        &CompileInputs::new(),
        RunSettings::default(),
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

/// The host's observation tool. `failing` makes every call answer with a
/// tool error, for the arm that pins an errored call not counting.
fn observation_tool(failing: bool) -> HostTools {
    HostTools::new().with(move |_| {
        let tool = if failing {
            RegisteredTool::function(
                OBSERVATION_TOOL_NAME,
                "Record one durable observation from this stage.",
                json!({
                    "type": "object",
                    "properties": {
                        "category": {"type": "string", "enum": ["painpoint", "note"]},
                        "text": {"type": "string"}
                    },
                    "required": ["category", "text"],
                }),
                |_, _| async {
                    Err(pebble_coding_agent::tools::ToolError::execution(
                        "the recording surface is down",
                    ))
                },
            )
        } else {
            RegisteredTool::function(
                OBSERVATION_TOOL_NAME,
                "Record one durable observation from this stage.",
                json!({
                    "type": "object",
                    "properties": {
                        "category": {"type": "string", "enum": ["painpoint", "note"]},
                        "text": {"type": "string"}
                    },
                    "required": ["category", "text"],
                }),
                |_, _| async { Ok("recorded".to_owned()) },
            )
        };
        vec![tool.with_source(ToolSource::Application)]
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

async fn run(rt: &Runtime, graph: Graph) -> ExecutionReport {
    host::run_configured(rt, HostRun::new(graph), |_, _| {})
        .await
        .expect("the run completes")
}

/// A required node that never calls the tool is invalid, with a failure
/// that teaches the fix.
#[tokio::test]
async fn a_required_node_without_a_call_fails_with_the_teaching_reason() {
    let dir = RunDir::new("observation-absent");
    let (client, _) = scripted_client(vec![ScriptedCall::response(text_response("Done."))]);
    let rt = runtime(dir.path(), client, observation_tool(false));
    let report = run(&rt, graph(true)).await;
    let output = output_of(&report, "a");
    assert_eq!(output["outcome"], json!("failed"), "{output}");
    assert_eq!(output["failure_class"], json!("bad_output"), "{output}");
    let reason = output["failure_reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains(OBSERVATION_TOOL_NAME),
        "the reason names the tool to call: {reason}"
    );
    assert!(
        reason.contains("nothing durable to record"),
        "the reason teaches the explicit empty entry: {reason}"
    );
}

/// One completed call satisfies the contract; the counter rides the
/// metrics.
#[tokio::test]
async fn one_completed_call_satisfies_the_contract() {
    let dir = RunDir::new("observation-recorded");
    let (client, _) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            OBSERVATION_TOOL_NAME,
            "1",
            json!({"category": "painpoint", "text": "the gate refused every push"}),
        )),
        ScriptedCall::response(text_response("Done.")),
    ]);
    let rt = runtime(dir.path(), client, observation_tool(false));
    let report = run(&rt, graph(true)).await;
    let output = output_of(&report, "a");
    assert_eq!(output["outcome"], json!("succeeded"), "{output}");
    let row = report
        .state
        .history()
        .iter()
        .find(|row| row.name == "a")
        .expect("agent outcome");
    assert_eq!(
        row.outcome.metrics.custom["pebble.observation_calls"], 1,
        "the counter rides the stage metrics"
    );
}

/// An errored call recorded nothing and does not satisfy the contract.
#[tokio::test]
async fn an_errored_call_does_not_count() {
    let dir = RunDir::new("observation-errored");
    let (client, _) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            OBSERVATION_TOOL_NAME,
            "1",
            json!({"category": "note", "text": "the surface is down"}),
        )),
        ScriptedCall::response(text_response("Done.")),
    ]);
    let rt = runtime(dir.path(), client, observation_tool(true));
    let report = run(&rt, graph(true)).await;
    let output = output_of(&report, "a");
    assert_eq!(output["outcome"], json!("failed"), "{output}");
    assert_eq!(output["failure_class"], json!("bad_output"), "{output}");
    let row = report
        .state
        .history()
        .iter()
        .find(|row| row.name == "a")
        .expect("agent outcome");
    let counter = &row.outcome.metrics.custom["pebble.observation_calls"];
    assert!(
        counter.is_null() || *counter == 0,
        "an errored call leaves the counter at zero: {counter}"
    );
}

/// A node without the attribute keeps the old posture: no call, no
/// failure.
#[tokio::test]
async fn a_node_without_the_attribute_needs_no_call() {
    let dir = RunDir::new("observation-unrequired");
    let (client, _) = scripted_client(vec![ScriptedCall::response(text_response("Done."))]);
    let rt = runtime(dir.path(), client, observation_tool(false));
    let report = run(&rt, graph(false)).await;
    assert_eq!(output_of(&report, "a")["outcome"], json!("succeeded"));
}

// The unused-import boundary differs between toolchain versions of the
// test kit; the provider handle is only needed by the scripted arms that
// read requests back, which these arms do not.
#[allow(unused)]
fn provider_type_witness(_: &ScriptedProvider) {}
