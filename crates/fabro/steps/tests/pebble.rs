//! Native sessions against a scripted model and real Petri execution scopes.

use std::collections::BTreeMap;
use std::future::pending;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use fabro_steps::pebble::PebbleClient;
use fabro_steps::pebble::environment::PebbleEnvironment;
use fabro_steps::register;
use ir::{CancelScopeId, Graph, RunStatus, ScopeId};
use lithos_llm::types::ReasoningEffort;
use pebble_coding_agent::environment::{Environment, EnvironmentErrorKind, ExecRequest};
use pebble_coding_agent::test_support::{
    EnvironmentContract, ScriptedCall, scripted_client, text_response, tool_call_response,
};
use pebble_coding_agent::tools::{OutputStream, ToolArtifact, ToolError, ToolOutputWriter};
use runtime::driver::{DeliverDisposition, EventObserver, ExecutionReport};
use runtime::engine::{EngineState, Event, EventRecord};
use runtime::executor::sandbox::HostExecutor;
use runtime::executor::{AcquireContext, Executor, Retention, ScopeOutcome, ScopeSpec};
use runtime::frontend::{CompileInputs, NoFiles};
use runtime::{RunOptions, Runtime};
use serde_json::{Value, json};
use smol_str::SmolStr;
use testkit::{RunDir, output_of};
use tokio::fs;
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

fn graph(extra: &str) -> Graph {
    let source = format!(
        r#"digraph T {{
        graph [backend="pebble", default_model="test/model"]
        start [shape=Mdiamond]
        a [prompt="Make the change and verify it" {extra}]
        exit [shape=Msquare]
        start -> a -> exit
    }}"#
    );
    let lowered = frontend_fabro::load("test.fabro", &source, &NoFiles, &CompileInputs::new());
    lowered.graph.expect("valid workflow")
}

fn runtime(dir: &RunDir, client: lithos_llm::Client) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(100);
    options.retention = Retention::Never;
    options.echo = false;
    register(Runtime::standard())
        .capability(PebbleClient(client))
        .options(options)
}

fn metrics(report: &ExecutionReport) -> &BTreeMap<SmolStr, Value> {
    &report
        .state
        .history()
        .iter()
        .find(|row| row.name == "a")
        .expect("agent outcome")
        .outcome
        .metrics
        .custom
}

#[tokio::test]
async fn environment_meets_the_pebble_contract() {
    let dir = RunDir::new("pebble-contract");
    let executor = HostExecutor::new(dir.path());
    let scope = ScopeSpec::new(ScopeId::new(1), "pebble").with_grace(Duration::from_millis(50));
    let handle = executor
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("scope");
    let environment = PebbleEnvironment::prepare(
        handle.exec(),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect("prepare");
    let contract = EnvironmentContract::new(&environment, "contract");
    contract.verify_files().await.expect("files");
    contract.verify_search().await.expect("search");
    contract.verify_commands().await.expect("commands");
    let path = "odd 'name; $(touch injected)\nfile";
    environment
        .write_file(path, "safe")
        .await
        .expect("write odd path");
    environment
        .rename_file(path, "moved 'name")
        .await
        .expect("move odd path");
    assert_eq!(
        environment
            .read_file_text("moved 'name")
            .await
            .expect("read"),
        "safe"
    );
    assert!(!environment.file_exists("injected").await.expect("exists"));
    assert!(
        executor
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
}

#[tokio::test]
async fn native_tools_edit_and_verify_in_the_scope() {
    let dir = RunDir::new("pebble-edit-test");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "write_file",
            "write",
            json!({"path":"answer.txt","content":"42\n"}),
        )),
        ScriptedCall::response(tool_call_response(
            "shell",
            "verify",
            json!({"command":"test \"$(cat answer.txt)\" = 42 && printf verified"}),
        )),
        ScriptedCall::response(text_response("Done and verified.")),
    ]);
    let report = runtime(&dir, client).run(graph("")).await.expect("replay");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "Done and verified.");
    assert_eq!(provider.requests().len(), 3);
    let requests = serde_json::to_string(&provider.requests()).expect("requests");
    assert!(requests.contains("verified"), "{requests}");
    assert_eq!(metrics(&report)["pebble.usage"]["input"], 30);
    assert_eq!(metrics(&report)["pebble.usage"]["output"], 15);
}

#[tokio::test]
async fn repairs_share_history_and_sum_accounting() {
    let dir = RunDir::new("pebble-repair");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(text_response("not JSON")),
        ScriptedCall::response(text_response(r#"{"value":42}"#)),
    ]);
    let report = runtime(&dir, client).run(graph(r#", output_schema="{\"type\":\"object\",\"required\":[\"value\"],\"properties\":{\"value\":{\"type\":\"integer\"}}}""#)).await.expect("replay");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["structured"]["value"], 42);
    assert_eq!(metrics(&report)["pebble.prompts"], 2);
    assert_eq!(metrics(&report)["pebble.usage"]["input"], 20);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        serde_json::to_string(&requests[1])
            .expect("request")
            .contains("not JSON")
    );
}

#[tokio::test]
async fn invalid_output_keeps_accounting() {
    let dir = RunDir::new("pebble-bad-output");
    let (client, _) = scripted_client(vec![ScriptedCall::response(text_response("not JSON"))]);
    let report = runtime(&dir, client)
        .run(graph(r#", output_schema="routing", output_retries=1"#))
        .await
        .expect("replay");
    assert_eq!(testkit::status_of(&report, "a").as_deref(), Some("failure"));
    assert_eq!(output_of(&report, "a")["failure_class"], "bad_output");
    assert_eq!(metrics(&report)["pebble.prompts"], 2);
    assert_eq!(metrics(&report)["pebble.usage"]["input"], 20);
}

#[tokio::test]
async fn cancellation_settles_the_prompt_and_preserves_usage() {
    let dir = RunDir::new("pebble-cancel");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "shell",
            "work",
            json!({"command":"printf ready"}),
        )),
        ScriptedCall::PendingOpen,
    ]);
    let rt = runtime(&dir, client);
    let driver = rt.driver(graph(""));
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    timeout(Duration::from_secs(15), async {
        provider.wait_for_call().await;
        provider.wait_for_call().await;
    })
    .await
    .expect("second call");
    handle.cancel(CancelScopeId::ROOT).await;
    let report = timeout(Duration::from_secs(10), run)
        .await
        .expect("cancel settles")
        .expect("run task");
    assert_ne!(report.status, RunStatus::Success);
    assert_eq!(metrics(&report)["pebble.usage"]["input"], 10);
    assert_eq!(metrics(&report)["pebble.prompts"], 1);
}

#[derive(Default)]
struct RecordedOutput {
    stdout: Mutex<Vec<u8>>,
    stderr: Mutex<Vec<u8>>,
}

#[async_trait::async_trait]
impl ToolOutputWriter for RecordedOutput {
    async fn append(&self, stream: OutputStream, bytes: &[u8]) -> Result<(), ToolError> {
        match stream {
            OutputStream::Stdout => self.stdout.lock().await.extend_from_slice(bytes),
            OutputStream::Stderr => self.stderr.lock().await.extend_from_slice(bytes),
            OutputStream::Result => panic!("process output has only two streams"),
        }
        Ok(())
    }
    async fn finish(&self) -> Result<Vec<ToolArtifact>, ToolError> {
        panic!("the caller owns finish")
    }
}

struct BrokenOutput {
    pending: bool,
}
#[async_trait::async_trait]
impl ToolOutputWriter for BrokenOutput {
    async fn append(&self, _: OutputStream, _: &[u8]) -> Result<(), ToolError> {
        if self.pending {
            pending().await
        } else {
            Err(ToolError::execution("storage failed"))
        }
    }
    async fn finish(&self) -> Result<Vec<ToolArtifact>, ToolError> {
        panic!("the caller owns finish")
    }
}

#[tokio::test]
async fn full_output_preserves_bytes_and_storage_failure_stops_the_process() {
    let dir = RunDir::new("pebble-output-storage");
    let executor = HostExecutor::new(dir.path());
    let scope = ScopeSpec::new(ScopeId::new(1), "pebble").with_grace(Duration::from_millis(50));
    let handle = executor
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("scope");
    let environment = PebbleEnvironment::prepare(
        handle.exec(),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect("prepare");
    let writer = Arc::new(RecordedOutput::default());
    let outcome = environment.exec(ExecRequest { output_bytes_cap: Some(12), output_writer: Some(writer.clone()), ..ExecRequest::new("printf 'HEAD\\r\\n'; printf '%0200000d' 0; printf '\\000\\377TAIL'; printf 'err\\r\\n\\000\\377' >&2") }).await.expect("capture");
    assert_eq!(outcome.stdout_capture.observed_bytes, 200_012);
    assert_eq!(outcome.stdout_capture.retained_bytes, 12);
    let stdout = writer.stdout.lock().await;
    assert_eq!(stdout.len(), 200_012);
    assert!(stdout.starts_with(b"HEAD\r\n"));
    assert!(stdout.ends_with(b"\0\xffTAIL"));
    assert_eq!(*writer.stderr.lock().await, b"err\r\n\0\xff");
    drop(stdout);
    for pending in [false, true] {
        let error = timeout(
            Duration::from_secs(10),
            environment.exec(ExecRequest {
                timeout_ms: Some(200),
                output_writer: Some(Arc::new(BrokenOutput { pending })),
                ..ExecRequest::new("echo $$ > worker.pid; printf ready; while :; do :; done")
            }),
        )
        .await
        .expect("capture settles")
        .expect_err("incomplete output must fail");
        assert_eq!(error.kind(), EnvironmentErrorKind::Io);
        let check = environment
            .exec(ExecRequest::new(
                "kill -0 \"$(cat worker.pid)\" 2>/dev/null",
            ))
            .await
            .expect("probe");
        assert!(
            !check.result.is_success(),
            "the failed writer's process was stopped"
        );
    }
    // The process can exit before a writer settles; its exit must not disable
    // the request's timeout while the last buffered chunk is still pending.
    timeout(
        Duration::from_secs(10),
        environment.exec(ExecRequest {
            timeout_ms: Some(200),
            output_writer: Some(Arc::new(BrokenOutput { pending: true })),
            ..ExecRequest::new("printf ready")
        }),
    )
    .await
    .expect("tail timeout settles")
    .expect_err("unfinished storage");
    assert!(
        executor
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
}

#[tokio::test]
async fn model_failure_keeps_prior_usage_and_known_cost() {
    use lithos_llm::types::{Cost, CostSource, ErrorKind};
    use pebble_coding_agent::test_support::ScriptedFailure;
    let dir = RunDir::new("pebble-model-error");
    let mut first = tool_call_response("shell", "work", json!({"command":"true"}));
    first.cost = Some(Cost {
        usd_micros: 123,
        source:     CostSource::Provider,
    });
    let (client, _) = scripted_client(vec![
        ScriptedCall::response(first),
        ScriptedCall::Failure(ScriptedFailure::terminal(
            ErrorKind::Authentication,
            "test failure",
        )),
    ]);
    let report = runtime(&dir, client).run(graph("")).await.expect("replay");
    assert_eq!(output_of(&report, "a")["failure_class"], "pebble_prompt");
    assert_eq!(metrics(&report)["pebble.usage"]["input"], 10);
    assert_eq!(metrics(&report)["pebble.cost_usd_micros"], 123);
}

#[test]
fn backend_selection_inherits_and_accepts_stylesheets() {
    for source in [
        r#"digraph T { graph [backend="pebble"]; start [shape=Mdiamond]; a [prompt="hello"]; exit [shape=Msquare]; start -> a -> exit; }"#,
        r#"digraph T { graph [model_stylesheet="* { backend: pebble; }"]; start [shape=Mdiamond]; a [prompt="hello"]; exit [shape=Msquare]; start -> a -> exit; }"#,
    ] {
        let result = frontend_fabro::load("test.fabro", source, &NoFiles, &CompileInputs::new());
        let graph = result
            .graph
            .unwrap_or_else(|| panic!("lowering failed: {:?}", result.diagnostics));
        let node = graph
            .body
            .nodes
            .iter()
            .find(|node| node.name == "a")
            .expect("agent");
        assert_eq!(node.step.config["backend"], "pebble");
    }
    let invalid = frontend_fabro::load(
        "test.fabro",
        r#"digraph T { start [shape=Mdiamond]; a [backend="invalid"]; exit [shape=Msquare]; start -> a -> exit; }"#,
        &NoFiles,
        &CompileInputs::new(),
    );
    assert!(invalid.graph.is_none());
    assert!(
        invalid
            .diagnostics
            .iter()
            .any(|d| d.code == "fabro.bad_backend")
    );
}

struct NativeStarted(mpsc::Sender<ir::FiringId>);
impl EventObserver for NativeStarted {
    fn on_record(&self, record: &EventRecord, _: &EngineState) {
        if let Event::StepProgress {
            firing,
            ev: ir::StepEvent::Custom(value),
        } = &record.event
            && value["kind"] == "pebble"
        {
            let _ = self.0.try_send(*firing);
        }
    }
}

#[tokio::test]
async fn steering_and_attributed_events_reach_the_native_session() {
    let dir = RunDir::new("pebble-steer");
    let gate = dir.path().join("release");
    let command = format!("while [[ ! -f '{}' ]]; do sleep 0.01; done", gate.display());
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "shell",
            "gate",
            json!({"command":command}),
        )),
        ScriptedCall::response(text_response("original answer")),
        ScriptedCall::response(text_response("steered answer")),
    ]);
    let (send, mut receive) = mpsc::channel(1);
    let rt = runtime(&dir, client).observe(Arc::new(NativeStarted(send)));
    let driver = rt.driver(graph(""));
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    let firing = timeout(Duration::from_secs(10), receive.recv())
        .await
        .expect("native event")
        .expect("firing");
    timeout(Duration::from_secs(10), provider.wait_for_call())
        .await
        .expect("first model call");
    assert_eq!(
        handle
            .deliver(
                firing,
                ir::Control::Deliver(json!({"text":"Check the edge cases too"}))
            )
            .await,
        DeliverDisposition::Delivered
    );
    fs::write(gate, "go").await.expect("release tool");
    let report = timeout(Duration::from_secs(15), run)
        .await
        .expect("run settles")
        .expect("task");
    assert_eq!(output_of(&report, "a")["text"], "steered answer");
    let requests = provider.requests();
    assert!(
        serde_json::to_string(&requests.last())
            .expect("request")
            .contains("Check the edge cases too")
    );
    let events: Vec<_> = report
        .state
        .log
        .events()
        .filter_map(|event| match event {
            Event::StepProgress {
                ev: ir::StepEvent::Custom(value),
                ..
            } if value["kind"] == "pebble" => Some(value),
            _ => None,
        })
        .collect();
    assert!(!events.is_empty());
    let stream = &events[0]["event"]["stream_id"];
    for (index, envelope) in events.iter().enumerate() {
        assert_eq!(envelope["node"], "a");
        assert_eq!(envelope["firing"], json!(firing));
        assert!(!envelope["attempt"].is_null());
        assert_eq!(envelope["event"]["seq"], json!(index + 1));
        assert_eq!(&envelope["event"]["stream_id"], stream);
        assert_eq!(&envelope["event"]["session_id"], stream);
    }
    assert!(
        events
            .iter()
            .any(|event| event["event"]["event"].get("ToolCallCompleted").is_some())
    );
}

#[tokio::test]
async fn kill_stops_a_tool_that_ignores_term() {
    let dir = RunDir::new("pebble-kill");
    let marker = dir.path().join("started");
    let command = format!(
        "trap '' TERM; echo $$ > '{}'; while :; do :; done",
        marker.display()
    );
    let (client, _) = scripted_client(vec![ScriptedCall::response(tool_call_response(
        "shell",
        "busy",
        json!({"command":command}),
    ))]);
    let (send, mut receive) = mpsc::channel(1);
    let rt = runtime(&dir, client).observe(Arc::new(NativeStarted(send)));
    let driver = rt.driver(graph(""));
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    let firing = timeout(Duration::from_secs(10), receive.recv())
        .await
        .expect("native event")
        .expect("firing");
    assert!(testkit::wait_for_file(&marker, Duration::from_secs(10)).await);
    let pid = fs::read_to_string(&marker).await.expect("pid");
    let _ = firing;
    handle.cancel(CancelScopeId::ROOT).await;
    handle.cancel(CancelScopeId::ROOT).await;
    let report = timeout(Duration::from_secs(5), run)
        .await
        .expect("kill settles")
        .expect("task");
    assert_eq!(
        testkit::status_of(&report, "a").as_deref(),
        Some("cancelled")
    );
    assert_eq!(metrics(&report)["pebble.usage"]["input"], 10);
    let status = Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(Stdio::null())
        .status()
        .await
        .expect("pid probe");
    assert!(!status.success(), "the tool process was reaped");
}

#[tokio::test]
async fn node_settings_select_the_actual_model_and_reasoning() {
    let dir = RunDir::new("pebble-model-settings");
    let (client, provider) = scripted_client(vec![ScriptedCall::response(text_response("done"))]);
    let report = runtime(&dir, client)
        .run(graph(
            r#", model="thinking", provider="test", reasoning_effort="high""#,
        ))
        .await
        .expect("replay");
    assert_eq!(testkit::status_of(&report, "a").as_deref(), Some("success"));
    let requests = provider.requests();
    assert_eq!(requests[0].model(), "test/thinking");
    assert_eq!(requests[0].reasoning_effort(), Some(ReasoningEffort::High));
}

#[tokio::test(flavor = "multi_thread")]
async fn environment_contract_runs_inside_a_container_without_host_files() {
    use runtime::executor::sandbox::RoutingExecutor;
    if !testkit::is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("pebble-docker-contract");
    let executor = RoutingExecutor::local(dir.path(), Retention::Never);
    let scope = ScopeSpec::new(ScopeId::new(1), "pebble-docker")
        .with_grace(Duration::from_millis(100))
        .with_runtime(ir::RuntimeSpec::container("buildpack-deps:noble"));
    let handle = executor
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("container");
    assert!(!handle.exec().shares_host_filesystem());
    let environment = PebbleEnvironment::prepare(
        handle.exec(),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect("prepare container");
    assert_eq!(environment.platform(), "linux");
    let contract = EnvironmentContract::new(&environment, "contract")
        .with_operation_timeout(Duration::from_secs(30));
    contract.verify_files().await.expect("container files");
    contract
        .verify_search()
        .await
        .expect("container search (grep fallback)");
    contract
        .verify_commands()
        .await
        .expect("container commands");
    assert!(
        executor
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
}

#[test]
fn mixed_backends_inherit_only_their_own_configuration() {
    let source = r#"digraph T {
        graph [acp.command="agent-command", default_model="test/model"]
        start [shape=Mdiamond]
        native [backend="pebble", prompt="native"]
        external [prompt="ACP"]
        exit [shape=Msquare]
        start -> native -> external -> exit
    }"#;
    let result = frontend_fabro::load("mixed.fabro", source, &NoFiles, &CompileInputs::new());
    let graph = result.graph.expect("mixed workflow");
    let native = graph
        .body
        .nodes
        .iter()
        .find(|node| node.name == "native")
        .expect("native");
    let external = graph
        .body
        .nodes
        .iter()
        .find(|node| node.name == "external")
        .expect("external");
    assert_eq!(native.step.config["backend"], "pebble");
    assert!(native.step.config.get("acp").is_none());
    assert_eq!(external.step.config["acp"]["command"], "agent-command");
}
