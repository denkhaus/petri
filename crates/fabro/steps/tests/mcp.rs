//! MCP servers on native sessions, readiness item 9b: `[run.agent.mcps]`
//! starts real servers (the scripted `mcp_server.py`), their tools reach the
//! model through Pebble's normal tool path, and every fact is a step event.
//! A scripted model, real Petri execution scopes, no Fabro.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs};

use fabro_steps::pebble::PebbleClient;
use fabro_steps::pebble::mcp::{SERVER_EVENT, TOOL_EVENT};
use fabro_steps::register;
use frontend::{CompileInputs, MapFiles};
use ir::{CancelScopeId, Graph, RunStatus, StepEvent, Value};
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedProvider, scripted_client, text_response, tool_call_response,
};
use runtime::driver::{EventObserver, ExecutionReport};
use runtime::engine::{EngineState, Event, EventRecord};
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, log_lines, output_of};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command as TokioCommand;
use tokio::time::{sleep, timeout};

/// The scripted server under `crates/fabro/acceptance/testdata`.
fn server_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../acceptance/testdata/mcp_server.py")
}

/// Every `StepEvent::Custom` the run emitted, with the node it came from.
#[derive(Default)]
struct Customs(Mutex<Vec<(String, Value)>>);

impl EventObserver for Customs {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, state: &EngineState) {
        if let Event::StepProgress {
            firing,
            ev: StepEvent::Custom(value),
        } = &record.event
        {
            let node = state
                .firing_node(*firing)
                .and_then(|id| state.graph().node(id))
                .map(|n| n.name.to_string())
                .unwrap_or_default();
            self.0
                .lock()
                .expect("not poisoned")
                .push((node, value.clone()));
        }
    }
}

impl Customs {
    fn all(&self) -> Vec<(String, Value)> {
        self.0.lock().expect("not poisoned").clone()
    }

    /// The server lifecycle events of `server`, as `(node, phase)`.
    fn phases(&self, server: &str) -> Vec<(String, String)> {
        self.all()
            .into_iter()
            .filter(|(_, v)| v["kind"] == SERVER_EVENT && v["server"] == server)
            .map(|(node, v)| (node, v["phase"].as_str().unwrap_or("?").to_owned()))
            .collect()
    }

    fn server_events(&self, server: &str, phase: &str) -> Vec<Value> {
        self.all()
            .into_iter()
            .filter(|(_, v)| {
                v["kind"] == SERVER_EVENT && v["server"] == server && v["phase"] == phase
            })
            .map(|(_, v)| v)
            .collect()
    }

    fn tool_events(&self) -> Vec<(String, Value)> {
        self.all()
            .into_iter()
            .filter(|(_, v)| v["kind"] == TOOL_EVENT)
            .collect()
    }

    /// The tool call statuses, in order.
    fn tool_statuses(&self) -> Vec<String> {
        self.tool_events()
            .into_iter()
            .map(|(_, v)| v["status"].as_str().unwrap_or("?").to_owned())
            .collect()
    }

    /// Pebble's own `ToolCallStarted` tool names, in order.
    fn pebble_tool_names(&self) -> Vec<String> {
        self.pebble_tool_event_names("ToolCallStarted")
    }

    /// Pebble's own `ToolCallCompleted` tool names, in order: the calls the
    /// session finished, which the mirrored tool events are derived from.
    fn pebble_completed_tool_names(&self) -> Vec<String> {
        self.pebble_tool_event_names("ToolCallCompleted")
    }

    fn pebble_tool_event_names(&self, variant: &str) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|(_, v)| v["kind"] == "pebble")
            .filter_map(|(_, v)| {
                v["event"]["event"][variant]["tool_name"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect()
    }
}

fn lower(dot: &str, toml: &str) -> Graph {
    let files = MapFiles(BTreeMap::from([(
        "wf/workflow.toml".to_string(),
        toml.to_string(),
    )]));
    let lowered = frontend_fabro::load("wf/w.fabro", dot, &files, &CompileInputs::new());
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        lowered.diagnostics
    );
    lowered.graph.expect("lowers")
}

fn runtime(dir: &RunDir, client: lithos_llm::Client) -> (Runtime, Arc<Customs>) {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Always;
    options.echo = false;
    let customs = Arc::new(Customs::default());
    let rt = Runtime::standard()
        .observe(customs.clone())
        .options(options)
        .capability(PebbleClient(client));
    (register(rt), customs)
}

async fn run(
    dir: &RunDir,
    graph: Graph,
    client: lithos_llm::Client,
) -> (ExecutionReport, Arc<Customs>) {
    let (rt, customs) = runtime(dir, client);
    let report = rt.run(graph).await.expect("replay is byte-identical");
    (report, customs)
}

fn workspace(dir: &RunDir) -> PathBuf {
    dir.path().join("scopes/scope-0/work")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// One agent node on the native backend.
fn agent_dot(attrs: &str) -> String {
    format!(
        r#"digraph W {{
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        agent [prompt="Use the notes server." {attrs}]
        start -> agent -> exit
    }}"#
    )
}

/// A `stdio` entry for the scripted server, tagged with the run dir so a
/// leaked process is attributable, logging its lifecycle to `log`.
fn stdio_entry(name: &str, dir: &RunDir, log: &Path, extra_args: &[&str], extra: &str) -> String {
    let mut args = vec![
        "python3".to_owned(),
        server_script().display().to_string(),
        "--tag".to_owned(),
        dir.path().display().to_string(),
    ];
    args.extend(extra_args.iter().map(|arg| (*arg).to_owned()));
    let command = serde_json::to_string(&args).expect("argv");
    format!(
        "[run.agent.mcps.{name}]\ntype = \"stdio\"\ncommand = {command}\nenv = {{ MCP_TEST_LOG = {:?} }}\n{extra}\n",
        log.display()
    )
}

fn call(id: &str, tool: &str, arguments: Value) -> ScriptedCall {
    ScriptedCall::response(tool_call_response(tool, id, arguments))
}

fn requests_text(provider: &ScriptedProvider) -> Vec<String> {
    provider
        .requests()
        .iter()
        .map(|request| serde_json::to_string(request).expect("request"))
        .collect()
}

fn process_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Wait for a process to disappear: a killed child is reaped by a task the
/// client library spawns on drop.
async fn wait_gone(pid: &str) {
    for _ in 0..100 {
        if !process_alive(pid) {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("process {pid} is still alive");
}

/// The pid an `echo __pid__` result carried, from the model's next request:
/// the first quoted all-digit string after the call's arguments.
fn pid_in(request: &str) -> String {
    let index = request
        .find("__pid__")
        .expect("the echo call is in the request");
    let after = &request[index..];
    after
        .split('"')
        .find(|part| part.len() >= 2 && part.chars().all(|c| c.is_ascii_digit()))
        .expect("a pid in the request")
        .to_owned()
}

async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    listener.local_addr().expect("address").port()
}

/// A configured `stdio` server starts while the agent is built, its tools
/// are registered under Fabro's qualified names with their MCP source, a call
/// writes into the scope's workspace, the result reaches the model, and the
/// server stops with the session, before the scope is released.
#[tokio::test]
async fn a_stdio_server_exposes_tools_that_act_on_the_workspace_and_stops_with_the_session() {
    let dir = RunDir::new("mcp-stdio");
    let log = dir.path().join("mcp.log");
    let (client, provider) = scripted_client(vec![
        call(
            "write",
            "mcp__notes__write_file",
            json!({"path": "note.txt", "content": "hello\n"}),
        ),
        call("cwd", "mcp__notes__echo", json!({"message": "__cwd__"})),
        call("pid", "mcp__notes__echo", json!({"message": "__pid__"})),
        ScriptedCall::response(text_response("Noted.")),
    ]);
    let graph = lower(&agent_dot(""), &stdio_entry("notes", &dir, &log, &[], ""));
    let (report, customs) = run(&dir, graph, client).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "agent")["text"], "Noted.");
    // The effect landed in the workspace: the server's working directory is
    // the scope's workspace on a host-backed scope.
    let ws = workspace(&dir);
    assert_eq!(read(&ws.join("note.txt")), "hello\n");
    let requests = requests_text(&provider);
    assert_eq!(requests.len(), 4);
    assert!(
        requests[1].contains("wrote 6 bytes to note.txt"),
        "the result reached the model: {}",
        requests[1]
    );
    let canonical = ws.canonicalize().expect("workspace");
    assert!(
        requests[2].contains(&canonical.display().to_string())
            || requests[2].contains(&ws.display().to_string()),
        "the server ran in the workspace: {}",
        requests[2]
    );
    let pid = pid_in(&requests[3]);
    wait_gone(&pid).await;
    // The server saw the whole lifecycle and a clean shutdown (EOF).
    assert_eq!(
        read(&log),
        "started\ninitialize\ncall write_file\ncall echo\ncall echo\nshutdown\n"
    );
    // Lifecycle and tool events, attributed to the node.
    assert_eq!(customs.phases("notes"), [
        ("agent".to_owned(), "starting".to_owned()),
        ("agent".to_owned(), "ready".to_owned()),
        ("agent".to_owned(), "stopped".to_owned()),
    ]);
    let ready = &customs.server_events("notes", "ready")[0];
    assert_eq!(ready["transport"], "stdio");
    assert_eq!(ready["placement"], "host");
    assert_eq!(ready["tool_count"], 6);
    let names: Vec<&str> = ready["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, [
        "mcp__notes__crash",
        "mcp__notes__echo",
        "mcp__notes__fail",
        "mcp__notes__read_file",
        "mcp__notes__sleep",
        "mcp__notes__write_file",
    ]);
    assert_eq!(ready["tools"][5]["original_name"], "write_file");
    // Pebble's launch-to-tools-listed time rides on `ready`.
    assert!(ready["duration_ms"].is_u64(), "{ready}");
    let tools = customs.tool_events();
    assert_eq!(customs.tool_statuses(), ["ok", "ok", "ok"]);
    assert_eq!(tools[0].0, "agent");
    assert_eq!(tools[0].1["server"], "notes");
    assert_eq!(tools[0].1["name"], "mcp__notes__write_file");
    assert_eq!(tools[0].1["tool"], "write_file");
    assert_eq!(tools[0].1["tool_call_id"], "write");
    assert!(tools[0].1["duration_ms"].is_u64());
    assert!(tools[0].1["error"].is_null());
    // Pebble ran the calls through its own tool path under the MCP name.
    assert_eq!(customs.pebble_tool_names(), [
        "mcp__notes__write_file",
        "mcp__notes__echo",
        "mcp__notes__echo",
    ]);
}

/// A configured `pre_tool_use` hook blocks an MCP tool: the call never
/// reaches the server, the model sees the reason, and a later call runs.
#[tokio::test]
async fn a_pre_tool_use_hook_blocks_an_mcp_tool_before_it_reaches_the_server() {
    let dir = RunDir::new("mcp-hook-block");
    let log = dir.path().join("mcp.log");
    let (client, provider) = scripted_client(vec![
        call(
            "secret",
            "mcp__notes__write_file",
            json!({"path": "secret.txt", "content": "leak"}),
        ),
        call(
            "ok",
            "mcp__notes__write_file",
            json!({"path": "ok.txt", "content": "fine"}),
        ),
        ScriptedCall::response(text_response("Done.")),
    ]);
    let toml = format!(
        r#"{}
[[run.hooks]]
name = "no-secrets"
event = "pre_tool_use"
matcher = "^mcp__notes__"
script = "if grep -q secret.txt \"$FABRO_HOOK_CONTEXT\"; then echo '{{\"decision\":\"block\",\"reason\":\"secret files are off limits\"}}'; exit 2; fi"

[[run.hooks]]
name = "after"
event = "post_tool_use"
matcher = "^mcp__"
script = "echo post:$FABRO_NODE_ID >> tool-hooks.log"
"#,
        stdio_entry("notes", &dir, &log, &[], "")
    );
    let graph = lower(&agent_dot(""), &toml);
    let (report, customs) = run(&dir, graph, client).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let ws = workspace(&dir);
    assert!(
        !ws.join("secret.txt").exists(),
        "the blocked call never ran"
    );
    assert_eq!(read(&ws.join("ok.txt")), "fine");
    assert_eq!(read(&ws.join("tool-hooks.log")), "post:agent\n");
    let requests = requests_text(&provider);
    assert!(
        requests[1].contains("secret files are off limits"),
        "{}",
        requests[1]
    );
    // The server saw one call; the blocked one produced no MCP tool event.
    assert_eq!(
        read(&log),
        "started\ninitialize\ncall write_file\nshutdown\n"
    );
    assert_eq!(customs.tool_statuses(), ["ok"]);
    assert_eq!(customs.tool_events()[0].1["tool_call_id"], "ok");
}

/// A result the server marks as an error, a call that outlives the tool
/// timeout, and a call a crashed server cannot answer each reach the model
/// with their reason; the crash is reported as a disconnection once and
/// later calls to that server fail at once. The slow call goes to its own
/// server: a timed-out call leaves the scripted server busy until it
/// finishes.
#[tokio::test]
async fn error_results_timeouts_and_a_crashed_server_reach_the_model_with_reasons() {
    let dir = RunDir::new("mcp-failures");
    let log = dir.path().join("mcp.log");
    let slow_log = dir.path().join("slow.log");
    let (client, provider) = scripted_client(vec![
        call("fail", "mcp__notes__fail", json!({"message": "disk full"})),
        call("slow", "mcp__slow__sleep", json!({"ms": 3000})),
        call("crash", "mcp__notes__crash", json!({})),
        call(
            "after",
            "mcp__notes__echo",
            json!({"message": "anyone there"}),
        ),
        ScriptedCall::response(text_response("Gave up.")),
    ]);
    let toml = format!(
        "{}{}",
        stdio_entry("notes", &dir, &log, &[], ""),
        stdio_entry("slow", &dir, &slow_log, &[], "tool_timeout = \"1s\"")
    );
    let graph = lower(&agent_dot(""), &toml);
    let (report, customs) = run(&dir, graph, client).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "agent")["text"], "Gave up.");
    let requests = requests_text(&provider);
    assert!(requests[1].contains("disk full"), "{}", requests[1]);
    assert!(
        requests[2].contains("did not answer within 1s"),
        "{}",
        requests[2]
    );
    assert!(
        requests[3].contains("failed the call to `crash`"),
        "{}",
        requests[3]
    );
    assert!(
        requests[4].contains("connection is closed"),
        "{}",
        requests[4]
    );
    // A timeout has its own status; the model and the event both get
    // Pebble's reason.
    assert_eq!(customs.tool_statuses(), [
        "error", "timeout", "failed", "failed"
    ]);
    let tools = customs.tool_events();
    assert_eq!(tools[0].1["error"], "disk full");
    assert!(
        tools[1].1["error"]
            .as_str()
            .is_some_and(|error| error.contains("did not answer within 1s")),
        "{}",
        tools[1].1
    );
    assert!(
        tools[2].1["error"]
            .as_str()
            .is_some_and(|error| error.contains("failed the call to `crash`")),
        "{}",
        tools[2].1
    );
    assert!(
        tools.iter().all(|(_, event)| event["duration_ms"].is_u64()),
        "{tools:?}"
    );
    // The crash is one `disconnected`, from the call that found the
    // connection closed, before that call's own tool event.
    assert_eq!(customs.phases("notes"), [
        ("agent".to_owned(), "starting".to_owned()),
        ("agent".to_owned(), "ready".to_owned()),
        ("agent".to_owned(), "disconnected".to_owned()),
        ("agent".to_owned(), "stopped".to_owned()),
    ]);
    let disconnected = &customs.server_events("notes", "disconnected")[0];
    assert!(disconnected["error"].is_string(), "{disconnected}");
    let order: Vec<(String, String)> = customs
        .all()
        .into_iter()
        .filter(|(_, v)| {
            (v["kind"] == SERVER_EVENT && v["server"] == "notes" && v["phase"] == "disconnected")
                || (v["kind"] == TOOL_EVENT && v["tool_call_id"] == "crash")
        })
        .map(|(_, v)| {
            (
                v["kind"].as_str().unwrap_or("?").to_owned(),
                v["phase"]
                    .as_str()
                    .or(v["status"].as_str())
                    .unwrap_or("?")
                    .to_owned(),
            )
        })
        .collect();
    assert_eq!(order, [
        (SERVER_EVENT.to_owned(), "disconnected".to_owned()),
        (TOOL_EVENT.to_owned(), "failed".to_owned()),
    ]);
    assert_eq!(
        read(&log),
        "started\ninitialize\ncall fail\ncall crash\ncrash\n"
    );
    let phases: Vec<String> = customs
        .phases("slow")
        .into_iter()
        .map(|(_, phase)| phase)
        .collect();
    assert_eq!(phases, ["starting", "ready", "stopped"]);
}

/// Cancelling the run while a call waits on the server ends the call, the
/// session and the server process.
#[tokio::test]
async fn cancellation_ends_a_slow_call_and_the_server_process() {
    let dir = RunDir::new("mcp-cancel");
    let log = dir.path().join("mcp.log");
    let (client, provider) = scripted_client(vec![
        call("pid", "mcp__notes__echo", json!({"message": "__pid__"})),
        call("slow", "mcp__notes__sleep", json!({"ms": 60000})),
        ScriptedCall::PendingOpen,
    ]);
    let graph = lower(&agent_dot(""), &stdio_entry("notes", &dir, &log, &[], ""));
    let (rt, customs) = runtime(&dir, client);
    let driver = rt.driver(graph);
    let handle = driver.handle();
    let running = tokio::spawn(driver.run());
    timeout(Duration::from_secs(20), async {
        provider.wait_for_call().await;
        provider.wait_for_call().await;
    })
    .await
    .expect("the slow call started");
    // The server is inside `sleep`; give the call a moment to be in flight.
    sleep(Duration::from_millis(300)).await;
    handle.cancel(CancelScopeId::ROOT).await;
    let report = timeout(Duration::from_secs(20), running)
        .await
        .expect("cancel settles")
        .expect("run task");
    assert_ne!(report.status, RunStatus::Success);
    let requests = requests_text(&provider);
    let pid = pid_in(&requests[1]);
    wait_gone(&pid).await;
    assert_eq!(customs.tool_statuses(), ["ok", "cancelled"]);
    let phases: Vec<String> = customs
        .phases("notes")
        .into_iter()
        .map(|(_, phase)| phase)
        .collect();
    assert_eq!(phases, ["starting", "ready", "stopped"]);
}

/// A retained thread: the second `full` node continues the conversation and
/// starts its own servers again, so the tool names the history carries are
/// registered for it too.
#[tokio::test]
async fn a_retained_session_registers_the_tools_again_for_the_next_node() {
    let dir = RunDir::new("mcp-retained");
    let log = dir.path().join("mcp.log");
    let (client, provider) = scripted_client(vec![
        call("one", "mcp__notes__echo", json!({"message": "first note"})),
        ScriptedCall::response(text_response("PLAN: keep notes")),
        call("two", "mcp__notes__echo", json!({"message": "second note"})),
        ScriptedCall::response(text_response("DONE")),
    ]);
    let dot = r#"digraph W {
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        plan [prompt="Plan.", fidelity="full", thread_id="notes"]
        implement [prompt="Implement.", fidelity="full", thread_id="notes"]
        start -> plan -> implement -> exit
    }"#;
    let graph = lower(dot, &stdio_entry("notes", &dir, &log, &[], ""));
    let (report, customs) = run(&dir, graph, client).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "implement")["text"], "DONE");
    let requests = requests_text(&provider);
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2].contains("first note") && requests[2].contains("PLAN: keep notes"),
        "the second node continues the conversation: {}",
        requests[2]
    );
    assert!(requests[3].contains("second note"), "{}", requests[3]);
    assert_eq!(customs.phases("notes"), [
        ("plan".to_owned(), "starting".to_owned()),
        ("plan".to_owned(), "ready".to_owned()),
        ("plan".to_owned(), "stopped".to_owned()),
        ("implement".to_owned(), "starting".to_owned()),
        ("implement".to_owned(), "ready".to_owned()),
        ("implement".to_owned(), "stopped".to_owned()),
    ]);
    let tools = customs.tool_events();
    assert_eq!(
        tools
            .iter()
            .map(|(node, v)| (node.as_str(), v["status"].as_str().unwrap_or("?")))
            .collect::<Vec<_>>(),
        [("plan", "ok"), ("implement", "ok")]
    );
    // Two server lifetimes, one per node.
    assert_eq!(
        read(&log),
        "started\ninitialize\ncall echo\nshutdown\nstarted\ninitialize\ncall echo\nshutdown\n"
    );
}

/// A server that does not start is reported with the reason and skipped;
/// the session continues with the servers that did start, as Fabro does.
#[tokio::test]
async fn a_server_that_fails_to_start_is_reported_and_the_others_serve() {
    let dir = RunDir::new("mcp-start-failure");
    let log = dir.path().join("mcp.log");
    let (client, provider) = scripted_client(vec![
        call("echo", "mcp__notes__echo", json!({"message": "still here"})),
        ScriptedCall::response(text_response("Fine.")),
    ]);
    let toml = format!(
        "{}{}[run.agent.mcps.missing]\ntype = \"stdio\"\ncommand = [\"/nonexistent/mcp-server\"]\n[run.agent.mcps.slow]\ntype = \"stdio\"\ncommand = [\"python3\", {:?}, \"--slow-init\", \"5000\"]\nstartup_timeout = \"1s\"\n",
        stdio_entry("broken", &dir, &log, &["--fail-init"], ""),
        stdio_entry("notes", &dir, &log, &[], ""),
        server_script().display().to_string()
    );
    let graph = lower(&agent_dot(""), &toml);
    let (report, customs) = run(&dir, graph, client).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(requests_text(&provider)[1].contains("still here"));
    assert_eq!(
        customs
            .phases("broken")
            .into_iter()
            .map(|(_, p)| p)
            .collect::<Vec<_>>(),
        ["starting", "failed"]
    );
    let broken = &customs.server_events("broken", "failed")[0];
    let error = broken["error"].as_str().expect("error");
    assert!(
        error.contains("refusing to start: --fail-init"),
        "the server's stderr is in the reason: {error}"
    );
    let missing = &customs.server_events("missing", "failed")[0];
    assert!(
        missing["error"]
            .as_str()
            .expect("error")
            .contains("could not launch `/nonexistent/mcp-server`"),
        "{missing}"
    );
    let slow = &customs.server_events("slow", "failed")[0];
    assert!(
        slow["error"]
            .as_str()
            .expect("error")
            .contains("did not complete the MCP handshake within 1s"),
        "{slow}"
    );
    // Pebble reported each failure with its launch-to-failure time; the
    // handshake timeout took at least its `startup_timeout`.
    for failed in [broken, missing, slow] {
        assert!(failed["duration_ms"].is_u64(), "{failed}");
    }
    assert!(
        slow["duration_ms"].as_u64().is_some_and(|ms| ms >= 1_000),
        "{slow}"
    );
    assert_eq!(
        customs
            .phases("notes")
            .into_iter()
            .map(|(_, p)| p)
            .collect::<Vec<_>>(),
        ["starting", "ready", "stopped"]
    );
    let lines = log_lines(&report);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("mcp server `broken` failed to start:")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("mcp server `missing` failed to start:")),
        "{lines:?}"
    );
}

/// The `http` transport reaches a server the run does not own, and the
/// `sandbox` transport launches one in the scope and reaches it on its port;
/// the owned one stops with the session. The run has ended when the events
/// are read, and each mirrored tool event is checked against the Pebble
/// completion it was derived from: the previous MCP client's test lost a
/// tool event to timing on macOS CI.
#[tokio::test]
async fn http_and_sandbox_transports_reach_a_server_on_a_port() {
    let dir = RunDir::new("mcp-http");
    let http_port = free_port().await;
    let sandbox_port = free_port().await;
    let mut remote = TokioCommand::new("python3")
        .arg(server_script())
        .args(["--http", &http_port.to_string()])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("the remote server starts");
    // Pebble connects to an `http` server once, with no readiness probe (only
    // an `Environment` placement polls until `startup_timeout`), so wait for
    // the server to listen: on the macOS runner the connect raced its start.
    wait_for_port(http_port).await;
    let (client, provider) = scripted_client(vec![
        call(
            "remote",
            "mcp__remote__echo",
            json!({"message": "over http"}),
        ),
        call("scope", "mcp__scoped__echo", json!({"message": "__pid__"})),
        ScriptedCall::response(text_response("Reached both.")),
    ]);
    let log = dir.path().join("mcp.log");
    let trace = dir.path().join("mcp.trace");
    let toml = format!(
        "[run.agent.mcps.remote]\ntype = \"http\"\nurl = \"http://127.0.0.1:{http_port}\"\nheaders = {{ X-Case = \"mcp-http\" }}\n\n[run.agent.mcps.scoped]\ntype = \"sandbox\"\ncommand = [\"python3\", {:?}, \"--http\", \"{sandbox_port}\"]\nport = {sandbox_port}\nenv = {{ MCP_TEST_LOG = {:?}, MCP_TEST_TRACE = {:?} }}\nstartup_timeout = \"15s\"\n",
        server_script().display().to_string(),
        log.display().to_string(),
        trace.display().to_string()
    );
    let graph = lower(&agent_dot(""), &toml);
    let (report, customs) = run(&dir, graph, client).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let requests = requests_text(&provider);
    assert!(requests[1].contains("over http"), "{}", requests[1]);
    // Both calls completed on Pebble's side...
    assert_eq!(
        customs.pebble_completed_tool_names(),
        ["mcp__remote__echo", "mcp__scoped__echo"],
        "server events: {:?}",
        customs.server_events("scoped", "failed")
    );
    // ...and before reading the pid: a scoped call that failed leaves no pid
    // in the request, and the server events and the tool statuses say why.
    assert_eq!(
        customs.tool_statuses(),
        ["ok", "ok"],
        "scoped server events: {:?}; tool events: {:?}; server log: {:?}; server trace: {:?}; python3 on PATH: {:?}",
        customs.server_events("scoped", "failed"),
        customs.tool_events(),
        read(&log),
        read(&trace),
        env::var_os("PATH").map(|path| env::split_paths(&path)
            .map(|dir| dir.join("python3"))
            .filter(|candidate| candidate.is_file())
            .collect::<Vec<_>>())
    );
    let pid = pid_in(&requests[2]);
    wait_gone(&pid).await;
    let remote_ready = &customs.server_events("remote", "ready")[0];
    assert_eq!(remote_ready["placement"], "remote");
    assert_eq!(remote_ready["transport"], "http");
    let scoped_ready = &customs.server_events("scoped", "ready")[0];
    assert_eq!(scoped_ready["placement"], "scope");
    assert_eq!(scoped_ready["transport"], "sandbox");
    assert_eq!(
        customs
            .phases("scoped")
            .into_iter()
            .map(|(_, p)| p)
            .collect::<Vec<_>>(),
        ["starting", "ready", "stopped"]
    );
    let _ = remote.kill().await;
}

/// Wait until a server the test spawned accepts connections on `port`.
async fn wait_for_port(port: u16) {
    timeout(Duration::from_secs(15), async {
        while TcpStream::connect(("127.0.0.1", port)).await.is_err() {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the server listens");
}

/// `protocol = "sse"`, as Fabro accepts it: an `http` server the run does not
/// own is reached over the older SSE transport at its URL, and a `sandbox`
/// server that speaks it is launched in the scope and reached at `/sse`
/// under the route to its port, where Fabro reaches one. Both answer a call
/// through Pebble's tool path; the owned one stops with the session.
#[tokio::test]
async fn sse_servers_are_reached_at_their_stream_over_http_and_under_a_sandbox_route() {
    let dir = RunDir::new("mcp-sse");
    let http_port = free_port().await;
    let sandbox_port = free_port().await;
    let mut remote = TokioCommand::new("python3")
        .arg(server_script())
        .args(["--sse", &http_port.to_string()])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("the remote server starts");
    wait_for_port(http_port).await;
    let (client, provider) = scripted_client(vec![
        call(
            "remote",
            "mcp__remote__echo",
            json!({"message": "over sse"}),
        ),
        call("scope", "mcp__scoped__echo", json!({"message": "__pid__"})),
        ScriptedCall::response(text_response("Reached both.")),
    ]);
    let log = dir.path().join("mcp.log");
    let toml = format!(
        "[run.agent.mcps.remote]\ntype = \"http\"\nprotocol = \"sse\"\nurl = \"http://127.0.0.1:{http_port}/sse\"\nheaders = {{ X-Case = \"mcp-sse\" }}\n\n[run.agent.mcps.scoped]\ntype = \"sandbox\"\nprotocol = \"sse\"\ncommand = [\"python3\", {:?}, \"--sse\", \"{sandbox_port}\"]\nport = {sandbox_port}\nenv = {{ MCP_TEST_LOG = {:?} }}\nstartup_timeout = \"15s\"\n",
        server_script().display().to_string(),
        log.display().to_string()
    );
    let graph = lower(&agent_dot(""), &toml);
    let (report, customs) = run(&dir, graph, client).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let requests = requests_text(&provider);
    assert!(requests[1].contains("over sse"), "{}", requests[1]);
    assert_eq!(
        customs.pebble_completed_tool_names(),
        ["mcp__remote__echo", "mcp__scoped__echo"],
        "server events: {:?}",
        customs.server_events("scoped", "failed")
    );
    assert_eq!(
        customs.tool_statuses(),
        ["ok", "ok"],
        "scoped server events: {:?}; tool events: {:?}; server log: {:?}",
        customs.server_events("scoped", "failed"),
        customs.tool_events(),
        read(&log)
    );
    // The owned server answered on its stream and stopped with the session.
    let pid = pid_in(&requests[2]);
    wait_gone(&pid).await;
    assert_eq!(read(&log), "started\ninitialize\ncall echo\n");
    let remote_ready = &customs.server_events("remote", "ready")[0];
    assert_eq!(remote_ready["placement"], "remote");
    assert_eq!(remote_ready["transport"], "http");
    let scoped_ready = &customs.server_events("scoped", "ready")[0];
    assert_eq!(scoped_ready["placement"], "scope");
    assert_eq!(scoped_ready["transport"], "sandbox");
    assert_eq!(
        customs
            .phases("scoped")
            .into_iter()
            .map(|(_, p)| p)
            .collect::<Vec<_>>(),
        ["starting", "ready", "stopped"]
    );
    let _ = remote.kill().await;
}

/// The scripted server is versioned test data under the acceptance crate.
#[test]
fn the_scripted_server_is_versioned_test_data() {
    let script = server_script();
    assert!(script.exists(), "{}", script.display());
    let text = read(&script);
    assert!(text.contains("\"name\": \"write_file\""));
}
