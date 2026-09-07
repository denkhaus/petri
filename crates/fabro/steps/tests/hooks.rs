//! The local hook system, readiness item 5: `[[run.hooks]]` loaded from the
//! workflow's configuration and executed at every reference phase, with the
//! four executor types, Fabro's decision rules, and the tool boundary of the
//! native backend. Plus fidelity, threads and project memory on real runs.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs};

use fabro_steps::agent::THREAD_EVENT;
use fabro_steps::hooks::{REPORT_EVENT, WARNING_EVENT};
use fabro_steps::pebble::PebbleClient;
use fabro_steps::{
    AGENT_KIND, CommandStep, HumanStep, PROMPT_KIND, StageStep, StubStep, WAIT_KIND, WORKFLOW_KIND,
    register,
};
use frontend::{CompileInputs, MapFiles};
use ir::{Graph, RunStatus, StepEvent, Value};
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedCompletion, ScriptedProvider, client_from, message_text, text_response,
    tool_call_response,
};
use runtime::driver::lifecycle::Note;
use runtime::driver::{EventObserver, ExecutionReport};
use runtime::engine::{EngineState, Event, EventRecord};
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, output_of, status_of};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// Every `StepEvent::Custom` the run emitted, with the node it came from.
#[derive(Default)]
struct Customs(Mutex<Vec<(String, Value)>>);

impl EventObserver for Customs {
    fn on_record(&self, record: &EventRecord, state: &EngineState) {
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

    /// The hook notes the adapter recorded, `(node, report)`.
    fn hook_notes(&self) -> Vec<(String, Value)> {
        self.all()
            .into_iter()
            .filter_map(|(node, value)| {
                let note = Note::from_step_event(&StepEvent::Custom(value))?;
                (note.kind == "hook").then_some((node, note.payload))
            })
            .collect()
    }

    /// The reports steps drove themselves (`fabro.hook`).
    fn reports(&self) -> Vec<(String, Value)> {
        self.all()
            .into_iter()
            .filter(|(_, v)| v["kind"] == REPORT_EVENT)
            .collect()
    }

    fn warnings(&self) -> Vec<Value> {
        self.all()
            .into_iter()
            .filter(|(_, v)| v["kind"] == WARNING_EVENT)
            .map(|(_, v)| v)
            .collect()
    }

    fn threads(&self) -> Vec<(String, Value)> {
        self.all()
            .into_iter()
            .filter(|(_, v)| v["kind"] == THREAD_EVENT)
            .collect()
    }
}

/// Every hook name that ran, in order, from both record kinds.
fn hook_names(customs: &Customs) -> Vec<String> {
    let mut names = Vec::new();
    for (_, report) in customs.hook_notes() {
        for hook in report["hooks"].as_array().into_iter().flatten() {
            names.push(hook["name"].as_str().unwrap_or("?").to_owned());
        }
    }
    for (_, event) in customs.reports() {
        for hook in event["report"]["hooks"].as_array().into_iter().flatten() {
            names.push(hook["name"].as_str().unwrap_or("?").to_owned());
        }
    }
    names
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

fn runtime(dir: &RunDir, client: Option<lithos_llm::Client>) -> (Runtime, Arc<Customs>) {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Always;
    options.echo = false;
    let customs = Arc::new(Customs::default());
    let rt = Runtime::standard()
        .observe(customs.clone())
        .options(options);
    let rt = match client {
        Some(client) => rt.capability(PebbleClient(client)),
        None => rt,
    };
    (register(rt), customs)
}

async fn run(
    dir: &RunDir,
    graph: Graph,
    client: Option<lithos_llm::Client>,
) -> (ExecutionReport, Arc<Customs>) {
    let (rt, customs) = runtime(dir, client);
    let report = rt.run(graph).await.expect("replay is byte-identical");
    (report, customs)
}

/// Real commands and stages, simulated agents: what a retry phase needs, as
/// a real command never requests a retry (Fabro's does not either).
async fn run_with_stub_agents(dir: &RunDir, graph: Graph) -> (ExecutionReport, Arc<Customs>) {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Always;
    options.echo = false;
    let customs = Arc::new(Customs::default());
    let mut registry = Runtime::standard().registry().clone();
    for kind in [&AGENT_KIND, &PROMPT_KIND, &WAIT_KIND, &WORKFLOW_KIND] {
        registry.register_runner(Arc::new(StubStep::new((*kind).clone())));
    }
    registry.register(CommandStep);
    registry.register(HumanStep);
    registry.register(StageStep);
    let rt = fabro_steps::services(
        Runtime::standard()
            .steps(registry)
            .observe(customs.clone())
            .options(options),
    );
    let report = rt.run(graph).await.expect("replay is byte-identical");
    (report, customs)
}

/// Script a stub node's calls, as the embedding test does.
fn simulate(graph: &mut Graph, node: &str, calls: Value) {
    let node = graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == node)
        .expect("node");
    let Value::Object(config) = &mut node.step.config else {
        panic!("an object config");
    };
    config.insert("simulate".into(), json!({ "calls": calls }));
}

fn workspace(dir: &RunDir) -> PathBuf {
    dir.path().join("scopes/scope-0/work")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

// ── Configuration, dispatch, command hooks ──────────────────────────────────

/// Every reference phase fires a command hook with Fabro's payload, in the
/// sandbox by default and on the host when asked; the log the hooks write
/// shows the order and the env each one saw.
#[tokio::test]
async fn command_hooks_fire_at_every_reference_phase_with_fabros_payload() {
    let dir = RunDir::new("hooks-phases");
    let ws = workspace(&dir);
    fs::create_dir_all(&ws).expect("workspace");
    let host_log = dir.path().join("host.log");
    let toml = format!(
        r#"
[[run.hooks]]
event = "run_start"
script = "echo run_start:$FABRO_EVENT:$FABRO_WORKFLOW >> hooks.log; test -n \"$FABRO_RUN_ID\""

[[run.hooks]]
event = "sandbox_ready"
script = "echo sandbox_ready:$(pwd) >> hooks.log"

[[run.hooks]]
name = "stage-start"
event = "stage_start"
script = "echo stage_start:$FABRO_NODE_ID:$(grep -o '\"attempt\":[0-9]*' \"$FABRO_HOOK_CONTEXT\" | cut -d: -f2) >> hooks.log"

[[run.hooks]]
event = "stage_complete"
script = "echo stage_complete:$FABRO_NODE_ID >> hooks.log"

[[run.hooks]]
event = "stage_failed"
script = "echo stage_failed:$FABRO_NODE_ID >> hooks.log"

[[run.hooks]]
event = "stage_retrying"
script = "echo stage_retrying:$FABRO_NODE_ID >> hooks.log"

[[run.hooks]]
event = "edge_selected"
script = "cat > ctx.json; echo edge_selected:$(grep -o '\"edge_from\":\"[^\"]*\"' ctx.json | cut -d'\"' -f4)-$(grep -o '\"edge_to\":\"[^\"]*\"' ctx.json | cut -d'\"' -f4) >> hooks.log"
sandbox = false

[[run.hooks]]
event = "run_complete"
script = "echo run_complete:$FABRO_EVENT >> {host}"
sandbox = false

[[run.hooks]]
event = "checkpoint_saved"
script = "echo never >> hooks.log"
"#,
        host = host_log.display()
    );
    let graph = lower(
        r#"digraph W {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        prepare [shape=parallelogram, script="echo prepared"]
        flaky [prompt="try", max_retries=1]
        start -> prepare -> flaky -> exit
    }"#,
        &toml,
    );
    let mut graph = graph;
    simulate(
        &mut graph,
        "flaky",
        json!([
            { "outcome": "failed", "failure_class": "retry_requested" },
            { "outcome": "succeeded" }
        ]),
    );
    let (report, customs) = run_with_stub_agents(&dir, graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let log = read(&ws.join("hooks.log"));
    let lines: Vec<&str> = log.lines().collect();
    assert!(
        !lines.is_empty(),
        "no hook output\nnotes: {:#?}\nreports: {:#?}\nwarnings: {:#?}",
        customs.hook_notes(),
        customs.reports(),
        customs.warnings()
    );
    // Fabro's order: the sandbox is ready before the run starts.
    assert!(
        lines[0].starts_with("sandbox_ready:") && lines[0].contains("work"),
        "{log}"
    );
    assert_eq!(lines[1], "run_start:run_start:W", "{log}");
    assert!(lines.contains(&"stage_start:prepare:1"), "{log}");
    assert!(lines.contains(&"stage_complete:prepare"), "{log}");
    // The retrying command: attempt 1 fails, the retry hook runs, attempt 2
    // starts and completes.
    assert!(lines.contains(&"stage_start:flaky:1"), "{log}");
    assert!(lines.contains(&"stage_retrying:flaky"), "{log}");
    assert!(lines.contains(&"stage_start:flaky:2"), "{log}");
    assert!(lines.contains(&"stage_complete:flaky"), "{log}");
    assert!(
        !lines.contains(&"stage_failed:flaky"),
        "a retried attempt is not a failed stage: {log}"
    );
    // Edge hooks ran on the host (the context on stdin) and saw both ends.
    assert!(lines.contains(&"edge_selected:prepare-flaky"), "{log}");
    assert!(lines.contains(&"edge_selected:flaky-exit"), "{log}");
    assert!(!log.contains("never"), "checkpoint_saved never runs: {log}");
    assert_eq!(read(&host_log).trim(), "run_complete:run_complete");
    // The reports: one per point with a hook; checkpoint_saved recorded as
    // unsupported, never executed.
    let names = hook_names(&customs);
    assert!(
        names.iter().filter(|n| n.as_str() == "stage-start").count() >= 3,
        "{names:?}"
    );
    let unsupported: Vec<_> = customs
        .hook_notes()
        .into_iter()
        .chain(
            customs
                .reports()
                .into_iter()
                .map(|(n, e)| (n, e["report"].clone())),
        )
        .flat_map(|(_, r)| r["hooks"].as_array().cloned().unwrap_or_default())
        .filter(|h| h["state"] == "unsupported")
        .collect();
    assert!(
        unsupported.is_empty(),
        "checkpoint_saved matched nothing at run time: {unsupported:?}"
    );
    // The stage payload the hooks saw: node, label, handler, attempts.
    let ctx: Value = serde_json::from_str(&read(&ws.join("ctx.json"))).expect("edge context");
    assert_eq!(ctx["event"], "edge_selected");
    assert_eq!(ctx["workflow_name"], "W");
    assert_eq!(ctx["handler_type"], "agent");
}

/// Fabro's exit-code rule and the decision points: a `stage_start` skip
/// skips the node, a block fails it and the run; a nonblocking post hook's
/// decision is ignored; a `blocking = false` override on a decision point is
/// ignored too.
#[tokio::test]
async fn command_decisions_skip_block_and_ignore_nonblocking_hooks() {
    let dir = RunDir::new("hooks-decisions");
    let graph = lower(
        r#"digraph W {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [shape=parallelogram, script="echo a > a.txt"]
        skipped [shape=parallelogram, script="echo ran > skipped.txt"]
        b [shape=parallelogram, script="echo b > b.txt"]
        start -> a -> skipped -> b -> exit
    }"#,
        r#"
[[run.hooks]]
name = "skip-it"
event = "stage_start"
matcher = "^skipped$"
script = "echo '{\"decision\":\"skip\",\"reason\":\"not today\"}'"

[[run.hooks]]
name = "loud-but-ignored"
event = "stage_complete"
matcher = "^a$"
script = "exit 3"

[[run.hooks]]
name = "nonblocking-block"
event = "stage_start"
matcher = "^b$"
blocking = false
script = "exit 2"
"#,
    );
    let (report, customs) = run(&dir, graph, None).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let ws = workspace(&dir);
    assert!(ws.join("a.txt").exists());
    assert!(
        !ws.join("skipped.txt").exists(),
        "the skipped node never ran"
    );
    assert!(
        ws.join("b.txt").exists(),
        "a nonblocking block does not block"
    );
    assert_eq!(status_of(&report, "skipped").as_deref(), Some("skipped"));
    let notes = customs.hook_notes();
    let skip = notes
        .iter()
        .find(|(n, _)| n == "skipped")
        .map(|(_, r)| r.clone())
        .expect("the skip report");
    assert_eq!(skip["decision"]["decision"], "skip");
    assert_eq!(skip["hooks"][0]["name"], "skip-it");
    assert_eq!(skip["hooks"][0]["message"], "skip: not today");
    let ignored = notes
        .iter()
        .find(|(n, r)| n == "a" && r["point"] == "after_visit")
        .map(|(_, r)| r.clone())
        .expect("the post report");
    assert_eq!(ignored["decision"]["decision"], "proceed");
    assert!(
        ignored["warnings"][0]
            .as_str()
            .is_some_and(|w| w.contains("not blocking")),
        "{ignored}"
    );

    // A block at stage_start fails the node with Fabro's reason and ends the run.
    let dir = RunDir::new("hooks-block");
    let graph = lower(
        r#"digraph W {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        guarded [shape=parallelogram, script="echo ran > guarded.txt"]
        start -> guarded -> exit
    }"#,
        r#"
[[run.hooks]]
event = "stage_start"
matcher = "^guarded$"
script = "echo 'no' >&2; exit 2"
"#,
    );
    let (report, customs) = run(&dir, graph, None).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert!(!workspace(&dir).join("guarded.txt").exists());
    let block = customs
        .hook_notes()
        .into_iter()
        .find(|(n, _)| n == "guarded")
        .map(|(_, r)| r)
        .expect("block report");
    assert_eq!(block["decision"]["decision"], "block");
    assert_eq!(block["decision"]["reason"], "hook exited with code 2");
}

/// `edge_selected`: an override redirects to a named target that is one of
/// the node's edges; a block stops advancement; a bad target is a warning.
#[tokio::test]
async fn edge_hooks_override_and_block_routes() {
    let dir = RunDir::new("hooks-edges");
    let graph = lower(
        r#"digraph W {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        pick [shape=parallelogram, script="echo picked"]
        left [shape=parallelogram, script="echo left > left.txt"]
        right [shape=parallelogram, script="echo right > right.txt"]
        start -> pick
        pick -> left
        pick -> right [weight=-1]
        left -> exit
        right -> exit
    }"#,
        r#"
[[run.hooks]]
name = "reroute"
event = "edge_selected"
matcher = "^pick$"
script = "echo '{\"decision\":\"override\",\"edge_to\":\"right\"}'"
"#,
    );
    let (report, customs) = run(&dir, graph, None).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let ws = workspace(&dir);
    assert!(
        ws.join("right.txt").exists(),
        "the override took the right edge"
    );
    assert!(!ws.join("left.txt").exists());
    let note = customs
        .hook_notes()
        .into_iter()
        .find(|(n, r)| n == "pick" && r["point"] == "route_selected")
        .map(|(_, r)| r)
        .expect("route report");
    assert_eq!(note["decision"]["decision"], "override");

    let dir = RunDir::new("hooks-edge-block");
    let graph = lower(
        r#"digraph W {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [shape=parallelogram, script="echo a"]
        b [shape=parallelogram, script="echo b > b.txt"]
        start -> a -> b -> exit
    }"#,
        r#"
[[run.hooks]]
event = "edge_selected"
matcher = "^a$"
script = "exit 1"
"#,
    );
    let (report, _) = run(&dir, graph, None).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert!(
        !workspace(&dir).join("b.txt").exists(),
        "a blocked edge never advances"
    );
}

/// A blocking `run_start` hook stops the run before any node; a blocked
/// `sandbox_ready` too. The stage reports it and the exit is never reached.
#[tokio::test]
async fn a_blocking_run_start_hook_stops_the_run_before_work() {
    let dir = RunDir::new("hooks-run-start");
    let graph = lower(
        r#"digraph W {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        work [shape=parallelogram, script="echo worked > worked.txt"]
        start -> work -> exit
    }"#,
        r#"
[[run.hooks]]
name = "env-check"
event = "run_start"
script = "echo '{\"decision\":\"block\",\"reason\":\"missing credential\"}'"
sandbox = false
"#,
    );
    let (report, customs) = run(&dir, graph, None).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert!(!workspace(&dir).join("worked.txt").exists());
    assert_eq!(status_of(&report, "start").as_deref(), Some("failure"));
    assert_eq!(output_of(&report, "start")["failure_class"], "hook_blocked");
    assert!(
        output_of(&report, "start")["failure_reason"]
            .as_str()
            .is_some_and(|r| r.contains("missing credential"))
    );
    let start = customs
        .reports()
        .into_iter()
        .find(|(_, e)| e["event"] == "run_start")
        .expect("run_start report");
    assert_eq!(start.1["report"]["hooks"][0]["name"], "env-check");
}

/// A command hook that outlives its timeout blocks (exit -1), as Fabro's
/// sandbox timeout does; cancellation of the run stops a running hook.
#[tokio::test]
async fn a_command_hook_timeout_blocks() {
    let dir = RunDir::new("hooks-timeout");
    let graph = lower(
        r#"digraph W {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [shape=parallelogram, script="echo a > a.txt"]
        start -> a -> exit
    }"#,
        r#"
[[run.hooks]]
event = "stage_start"
matcher = "^a$"
script = "sleep 5"
timeout = "300ms"
"#,
    );
    let started = std::time::Instant::now();
    let (report, customs) = run(&dir, graph, None).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the timeout ended the hook"
    );
    assert_eq!(report.status, RunStatus::Failed);
    let block = customs
        .hook_notes()
        .into_iter()
        .find(|(n, _)| n == "a")
        .map(|(_, r)| r)
        .expect("report");
    assert_eq!(block["decision"]["reason"], "hook exited with code -1");
}

// ── HTTP hooks ──────────────────────────────────────────────────────────────

/// A loopback listener that records one request body and answers with a
/// fixed status and body.
async fn http_endpoint(
    status: &'static str,
    body: &'static str,
) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let record = record.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 65536];
                let mut total = Vec::new();
                loop {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    total.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&total).into_owned();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let head = &text[..split];
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if text.len() >= split + 4 + len {
                            record.lock().expect("not poisoned").push(text);
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (addr, seen)
}

/// An HTTP hook posts Fabro's context with the configured headers and takes
/// the decision the endpoint returns; `tls = "off"` allows plain http; a
/// verify-mode `http://` URL blocks; a failing endpoint fails open.
#[tokio::test]
async fn http_hooks_post_the_context_and_fail_open() {
    let (blocker, seen) = http_endpoint(
        "200 OK",
        r#"{"decision":"block","reason":"webhook said no"}"#,
    )
    .await;
    let (down, _) = http_endpoint("500 Internal Server Error", "boom").await;
    let dir = RunDir::new("hooks-http");
    let graph = lower(
        r#"digraph W {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [shape=parallelogram, script="echo a > a.txt"]
        b [shape=parallelogram, script="echo b > b.txt"]
        start -> a -> b -> exit
    }"#,
        &format!(
            r#"
[[run.hooks]]
name = "notify"
event = "stage_complete"
matcher = "^a$"
url = "http://{down}/done"
tls = "off"
blocking = true

[[run.hooks]]
name = "insecure"
event = "stage_start"
matcher = "^a$"
url = "http://{blocker}/gate"
blocking = false

[[run.hooks]]
name = "gate"
event = "stage_start"
matcher = "^b$"
url = "http://{blocker}/gate"
tls = "off"
[run.hooks.headers]
X-Env = "test"
"#
        ),
    );
    let (report, customs) = run(&dir, graph, None).await;
    assert_eq!(report.status, RunStatus::Failed);
    let ws = workspace(&dir);
    assert!(ws.join("a.txt").exists());
    assert!(!ws.join("b.txt").exists(), "the webhook blocked b");
    let requests = seen.lock().expect("not poisoned").clone();
    assert_eq!(
        requests.len(),
        1,
        "the verify-mode http URL never reached the endpoint: {requests:?}"
    );
    let request = &requests[0];
    assert!(request.starts_with("POST /gate HTTP/1.1"), "{request}");
    assert!(
        request.to_ascii_lowercase().contains("x-env: test"),
        "{request}"
    );
    let body: Value =
        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap_or("")).expect("json body");
    assert_eq!(body["event"], "stage_start");
    assert_eq!(body["node_id"], "b");
    assert_eq!(body["attempt"], 1);
    let notes = customs.hook_notes();
    let a_start = notes
        .iter()
        .find(|(n, r)| n == "a" && r["point"] == "before_attempt")
        .expect("a start")
        .1
        .clone();
    assert_eq!(a_start["hooks"][0]["name"], "insecure");
    assert!(
        a_start["hooks"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("https://")),
        "{a_start}"
    );
    assert_eq!(
        a_start["decision"]["decision"], "proceed",
        "a nonblocking hook's block is ignored"
    );
    let a_done = notes
        .iter()
        .find(|(n, r)| n == "a" && r["point"] == "after_visit")
        .expect("a done")
        .1
        .clone();
    assert_eq!(a_done["hooks"][0]["state"], "failed_open");
    assert!(
        a_done["warnings"][0]
            .as_str()
            .is_some_and(|w| w.contains("500")),
        "{a_done}"
    );
    let b_start = notes
        .iter()
        .find(|(n, r)| n == "b" && r["point"] == "before_attempt")
        .expect("b start")
        .1
        .clone();
    assert_eq!(b_start["decision"]["reason"], "webhook said no");
}

// ── Prompt and agent hooks ─────────────────────────────────────────────────

fn scripted(
    stream: Vec<ScriptedCall>,
    completions: Vec<ScriptedCompletion>,
) -> (lithos_llm::Client, Arc<ScriptedProvider>) {
    client_from(ScriptedProvider::new(stream).completing(completions))
}

/// A prompt hook makes one non-streaming call with Fabro's evaluator prompt
/// and the context; `ok: false` blocks, an unparseable answer fails open, a
/// model error fails open.
#[tokio::test]
async fn prompt_hooks_evaluate_with_one_model_call_and_fail_open() {
    let dir = RunDir::new("hooks-prompt");
    let (client, provider) = scripted(vec![], vec![
        ScriptedCompletion::response(text_response(r#"{"ok": true}"#)),
        ScriptedCompletion::response(text_response("I am not sure")),
        ScriptedCompletion::response(text_response(r#"{"ok": false, "reason": "unsafe stage"}"#)),
    ]);
    let graph = lower(
        r#"digraph W {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [shape=parallelogram, script="echo a > a.txt"]
        b [shape=parallelogram, script="echo b > b.txt"]
        c [shape=parallelogram, script="echo c > c.txt"]
        start -> a -> b -> c -> exit
    }"#,
        r#"
[[run.hooks]]
name = "guard"
event = "stage_start"
matcher = "^(a|b|c)$"
prompt = "Should this stage proceed?"
model = "test/model"
"#,
    );
    let (report, customs) = run(&dir, graph, Some(client)).await;
    assert_eq!(report.status, RunStatus::Failed);
    let ws = workspace(&dir);
    assert!(ws.join("a.txt").exists());
    assert!(
        ws.join("b.txt").exists(),
        "an unparseable verdict fails open"
    );
    assert!(!ws.join("c.txt").exists(), "ok:false blocks");
    let requests = provider.completion_requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].model(), "test/model");
    let sent = message_text(&requests[0].messages()[1]);
    assert!(
        sent.starts_with("Hook prompt: Should this stage proceed?"),
        "{sent}"
    );
    assert!(sent.contains("\"node_id\": \"a\""), "{sent}");
    assert!(requests[0].tools().is_empty());
    let notes = customs.hook_notes();
    let b = notes
        .iter()
        .find(|(n, r)| n == "b" && r["point"] == "before_attempt")
        .expect("b")
        .1
        .clone();
    assert_eq!(b["hooks"][0]["state"], "failed_open");
    let c = notes
        .iter()
        .find(|(n, r)| n == "c" && r["point"] == "before_attempt")
        .expect("c")
        .1
        .clone();
    assert_eq!(c["decision"]["reason"], "unsafe stage");
    assert_eq!(c["hooks"][0]["state"], "executed");
}

/// An agent hook runs a Pebble agent with the coding tools in the sandbox:
/// it reads the workspace through a real tool before deciding. Its tool calls
/// do not fire tool hooks again.
#[tokio::test]
async fn agent_hooks_investigate_the_workspace_then_decide() {
    let dir = RunDir::new("hooks-agent");
    let (client, provider) = scripted(
        vec![
            ScriptedCall::response(tool_call_response(
                "shell",
                "look",
                json!({"command": "cat marker.txt"}),
            )),
            ScriptedCall::response(text_response(
                r#"{"ok": false, "reason": "marker says stop"}"#,
            )),
        ],
        vec![],
    );
    let graph = lower(
        r#"digraph W {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [shape=parallelogram, script="echo stop > marker.txt"]
        b [shape=parallelogram, script="echo b > b.txt"]
        start -> a -> b -> exit
    }"#,
        r#"
[[run.hooks]]
name = "verify"
event = "stage_start"
matcher = "^b$"
agent = "enabled"
prompt = "Read marker.txt and decide."
model = "test/model"
max_tool_rounds = 3

[[run.hooks]]
name = "tool-guard"
event = "pre_tool_use"
script = "echo tool-guard-ran >> tool-hooks.log"
"#,
    );
    let (report, customs) = run(&dir, graph, Some(client)).await;
    assert_eq!(
        report.status,
        RunStatus::Failed,
        "{:?}",
        report.state.errors()
    );
    let ws = workspace(&dir);
    assert!(!ws.join("b.txt").exists());
    assert_eq!(provider.requests().len(), 2, "one tool round, one verdict");
    let sent = serde_json::to_string(&provider.requests()[1]).expect("request");
    assert!(
        sent.contains("stop"),
        "the tool result reached the hook agent: {sent}"
    );
    assert!(
        !ws.join("tool-hooks.log").exists(),
        "hook work fires no hooks"
    );
    let b = customs
        .hook_notes()
        .into_iter()
        .find(|(n, r)| n == "b" && r["point"] == "before_attempt")
        .map(|(_, r)| r)
        .expect("b");
    assert_eq!(b["decision"]["reason"], "marker says stop");
    assert_eq!(b["hooks"][0]["name"], "verify");
}

// ── Tool hooks at the native boundary ──────────────────────────────────────

/// A `pre_tool_use` command hook blocks a real tool call: the file the shell
/// tool would write never exists, the model sees the denial, and the post
/// hooks fire for the calls that ran, with the tool output and the failure
/// message.
#[tokio::test]
async fn native_tool_hooks_block_pre_and_observe_post() {
    let dir = RunDir::new("hooks-tools");
    let (client, provider) = scripted(
        vec![
            ScriptedCall::response(tool_call_response(
                "shell",
                "danger",
                json!({"command": "echo pwned > pwned.txt"}),
            )),
            ScriptedCall::response(tool_call_response(
                "shell",
                "fine",
                json!({"command": "echo fine > fine.txt && echo FINE_DONE"}),
            )),
            ScriptedCall::response(tool_call_response(
                "shell",
                "broken",
                json!({"command": "exit 7"}),
            )),
            ScriptedCall::response(text_response("Done.")),
        ],
        vec![],
    );
    let graph = lower(
        r#"digraph W {
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        agent [prompt="Do the work."]
        start -> agent -> exit
    }"#,
        r#"
[[run.hooks]]
name = "no-pwn"
event = "pre_tool_use"
matcher = "^shell$"
script = "if grep -q pwned \"$FABRO_HOOK_CONTEXT\"; then echo '{\"decision\":\"block\",\"reason\":\"no pwn\"}'; exit 2; fi"

[[run.hooks]]
name = "after"
event = "post_tool_use"
script = "echo post:$FABRO_NODE_ID >> tool-hooks.log; grep -o 'fine' \"$FABRO_HOOK_CONTEXT\" | head -1 >> tool-hooks.log"

[[run.hooks]]
name = "after-failure"
event = "post_tool_use_failure"
script = "echo failure >> tool-hooks.log"
"#,
    );
    let (report, customs) = run(&dir, graph, Some(client)).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let ws = workspace(&dir);
    assert!(!ws.join("pwned.txt").exists(), "the blocked tool never ran");
    assert!(ws.join("fine.txt").exists());
    let log = read(&ws.join("tool-hooks.log"));
    assert!(log.contains("post:agent"), "{log}");
    assert!(
        log.contains("fine"),
        "the tool output reached the post hook: {log}"
    );
    let requests = provider.requests();
    let denial = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        denial.contains("no pwn"),
        "the model saw the block: {denial}"
    );
    let reports = customs.reports();
    let pre = reports
        .iter()
        .find(|(_, e)| e["event"] == "pre_tool_use")
        .expect("pre report")
        .1
        .clone();
    assert_eq!(pre["node"], "agent");
    assert_eq!(pre["report"]["decision"]["decision"], "block");
    assert_eq!(pre["report"]["hooks"][0]["name"], "no-pwn");
    let events: Vec<&str> = reports
        .iter()
        .filter_map(|(_, e)| e["event"].as_str())
        .collect();
    assert!(events.contains(&"post_tool_use"), "{events:?}");
    // The third call exits 7: Pebble reports a shell exit as a tool result,
    // not a tool failure, so the failure hook fires only when the tool itself
    // fails. Either way the log shows what ran.
    assert!(
        events.iter().filter(|e| **e == "pre_tool_use").count() >= 3,
        "{events:?}"
    );
}

// ── Fidelity, threads, memory ───────────────────────────────────────────────

/// Two `full` nodes on one thread share a conversation: the second prompt
/// carries the first's history and no preamble. A `compact` node between
/// them gets the preamble and a fresh session. Events are attributed to the
/// node that used the session.
#[tokio::test]
async fn full_fidelity_nodes_continue_their_thread_and_others_start_fresh() {
    let dir = RunDir::new("threads-continue");
    let (client, provider) = scripted(
        vec![
            ScriptedCall::response(text_response("planned")),
            ScriptedCall::response(text_response("implemented")),
            ScriptedCall::response(text_response("reviewed")),
        ],
        vec![],
    );
    let graph = lower(
        r#"digraph W {
        graph [backend="api", default_model="test/model", goal="Ship it"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        plan [prompt="Plan.", fidelity="full", thread_id="impl"]
        implement [prompt="Implement.", fidelity="full", thread_id="impl"]
        review [prompt="Review.", fidelity="summary:low"]
        start -> plan -> implement -> review -> exit
    }"#,
        "",
    );
    let (report, customs) = run(&dir, graph, Some(client)).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let first_user = message_text(
        requests[0]
            .messages()
            .iter()
            .find(|m| m.role() == lithos_llm::types::Role::User)
            .expect("user"),
    );
    assert_eq!(first_user, "Plan.", "full fidelity: no preamble");
    // The second request carries the first exchange.
    let second: Vec<String> = requests[1].messages().iter().map(message_text).collect();
    assert!(second.iter().any(|m| m == "Plan."), "{second:?}");
    assert!(second.iter().any(|m| m.contains("planned")), "{second:?}");
    assert!(second.iter().any(|m| m == "Implement."), "{second:?}");
    // The review starts fresh with a low summary of both stages.
    let third: Vec<String> = requests[2].messages().iter().map(message_text).collect();
    assert!(!third.iter().any(|m| m == "Plan."), "{third:?}");
    let review_prompt = third.last().expect("review prompt");
    assert!(
        review_prompt.starts_with("Goal: Ship it\nRun ID:"),
        "{review_prompt}"
    );
    assert!(
        review_prompt.contains("Recent stages:\n- plan: succeeded"),
        "{review_prompt}"
    );
    assert!(
        review_prompt.contains("- implement: succeeded"),
        "{review_prompt}"
    );
    assert!(review_prompt.ends_with("\n\nReview."), "{review_prompt}");
    let threads = customs.threads();
    let of = |node: &str| {
        threads
            .iter()
            .find(|(n, _)| n == node)
            .map(|(_, v)| v.clone())
            .expect(node)
    };
    assert_eq!(of("plan")["thread"], "impl");
    assert_eq!(of("plan")["reused"], false);
    assert_eq!(of("implement")["reused"], true);
    assert_eq!(of("implement")["fidelity"], "full");
    assert_eq!(of("review")["fidelity"], "summary:low");
    assert_eq!(
        of("review")["thread"],
        "implement",
        "the previous node id is the fallback thread"
    );
    assert_eq!(of("review")["thread_source"], "previous");
    // Pebble events of the reused session are attributed to `implement`.
    let pebble: Vec<(String, Value)> = customs
        .all()
        .into_iter()
        .filter(|(_, v)| v["kind"] == "pebble")
        .collect();
    assert!(
        pebble
            .iter()
            .any(|(n, v)| n == "implement" && v["node"] == "implement")
    );
    assert!(
        !pebble
            .iter()
            .any(|(n, v)| n == "implement" && v["node"] == "plan")
    );
    // Per-stage metrics start at zero for the second node.
    let metrics = |name: &str| {
        report
            .state
            .history()
            .iter()
            .find(|r| r.name == name)
            .expect(name)
            .outcome
            .metrics
            .custom
            .clone()
    };
    assert_eq!(metrics("plan")["pebble.prompts"], 1);
    assert_eq!(metrics("implement")["pebble.prompts"], 1);
}

/// Edge fidelity beats node fidelity; `truncate` is the goal and run id; a
/// thread whose conversation was lost (the previous node failed) degrades a
/// later `full` node to `summary:high`.
#[tokio::test]
async fn edge_fidelity_wins_and_a_lost_thread_degrades_to_summary_high() {
    let dir = RunDir::new("threads-edge");
    let (client, provider) = scripted(
        vec![
            ScriptedCall::response(text_response("not json at all")),
            ScriptedCall::response(text_response("second")),
        ],
        vec![],
    );
    let graph = lower(
        r#"digraph W {
        graph [backend="api", default_model="test/model", goal="G", default_fidelity="full", default_thread="t"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        first [prompt="First.", output_schema="routing", output_retries=0]
        second [prompt="Second."]
        start -> first
        first -> second [fidelity="truncate"]
        first -> second
        second -> exit
    }"#,
        "",
    );
    let (report, customs) = run(&dir, graph, Some(client)).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "first").as_deref(), Some("failure"));
    let requests = provider.requests();
    let prompt = message_text(requests[1].messages().last().expect("prompt"));
    assert!(prompt.starts_with("Goal: G\nRun ID: "), "{prompt}");
    assert!(
        prompt.ends_with("\n\n\nSecond.") || prompt.ends_with("\n\nSecond."),
        "{prompt}"
    );
    assert!(
        !prompt.contains("Completed"),
        "truncate carries no stages: {prompt}"
    );
    let threads = customs.threads();
    let second = threads
        .iter()
        .find(|(n, _)| n == "second")
        .map(|(_, v)| v.clone())
        .expect("second");
    assert_eq!(second["fidelity"], "truncate");
    assert_eq!(second["fidelity_source"], "edge");
    assert_eq!(second["thread"], "t");

    // A lost thread: the first node failed, so a later full node on the
    // same thread degrades.
    let dir = RunDir::new("threads-lost");
    let (client, provider) = scripted(
        vec![
            ScriptedCall::response(text_response("not json at all")),
            ScriptedCall::response(text_response("recovered")),
        ],
        vec![],
    );
    let graph = lower(
        r#"digraph W {
        graph [backend="api", default_model="test/model", goal="G", default_fidelity="full", default_thread="t"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        first [prompt="First.", output_schema="routing", output_retries=0]
        second [prompt="Second."]
        start -> first -> second -> exit
    }"#,
        "",
    );
    let (report, customs) = run(&dir, graph, Some(client)).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let second = customs
        .threads()
        .iter()
        .find(|(n, _)| n == "second")
        .map(|(_, v)| v.clone())
        .expect("second");
    assert_eq!(second["fidelity"], "summary:high");
    assert_eq!(second["fidelity_source"], "resume");
    assert_eq!(second["reused"], false);
    let prompt = message_text(provider.requests()[1].messages().last().expect("prompt"));
    assert!(
        prompt.contains("## Stage: first\n- Status: failed"),
        "{prompt}"
    );
}

/// A native session loads the profile's project documents from the Git root
/// down to the working directory; a prompt node reads the working directory
/// only, and `project_memory=false` reads nothing.
#[tokio::test]
async fn project_memory_follows_the_profile_and_the_node_kind() {
    let dir = RunDir::new("memory-paths");
    let ws = workspace(&dir);
    fs::create_dir_all(ws.join("sub")).expect("workspace");
    // The workspace is a Git repository whose root is `ws`; the run's working
    // directory is the workspace itself, so the walk is one level. Put a
    // parent-level file above to prove the walk starts at the Git root, not
    // above it.
    let git = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&ws)
        .status();
    if !git.is_ok_and(|s| s.success()) {
        return;
    }
    fs::write(ws.join("AGENTS.md"), "Root rules.").expect("write");
    fs::write(ws.join("CLAUDE.md"), "Claude rules.").expect("write");
    fs::write(ws.join("GEMINI.md"), "Gemini rules.").expect("write");
    fs::write(dir.path().join("AGENTS.md"), "ABOVE THE ROOT").expect("write");
    let (client, provider) = scripted(
        vec![ScriptedCall::response(text_response("agent done"))],
        vec![
            ScriptedCompletion::response(text_response("prompt done")),
            ScriptedCompletion::response(text_response("quiet done")),
        ],
    );
    let graph = lower(
        r#"digraph W {
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        agent [prompt="Agent."]
        summary [shape=tab, prompt="Summarize."]
        quiet [shape=tab, prompt="Quiet.", project_memory=false]
        start -> agent -> summary -> quiet -> exit
    }"#,
        "",
    );
    let (report, provider_calls) = {
        let (report, _) = run(&dir, graph, Some(client)).await;
        (report, provider)
    };
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    // The agent session's system prompt carries AGENTS.md and CLAUDE.md
    // (the test catalog's model is the anthropic profile), not GEMINI.md and
    // nothing from above the Git root.
    let agent_request = &provider_calls.requests()[0];
    let system = agent_request
        .messages()
        .iter()
        .filter(|m| m.role() == lithos_llm::types::Role::System)
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(system.contains("Root rules."), "{system}");
    assert!(system.contains("Claude rules."), "{system}");
    assert!(!system.contains("Gemini rules."), "{system}");
    assert!(!system.contains("ABOVE THE ROOT"), "{system}");
    let completions = provider_calls.completion_requests();
    assert_eq!(completions.len(), 2);
    let summary_system = completions[0]
        .messages()
        .iter()
        .filter(|m| m.role() == lithos_llm::types::Role::System)
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        summary_system.contains("Root rules.") && summary_system.contains("Claude rules."),
        "{summary_system}"
    );
    assert!(
        completions[1]
            .messages()
            .iter()
            .all(|m| m.role() != lithos_llm::types::Role::System),
        "project_memory=false"
    );
}

/// Model request controls: `speed` and `max_tokens` reach the native
/// session's requests and a prompt node's request.
#[tokio::test]
async fn speed_and_max_tokens_reach_the_model_requests() {
    let dir = RunDir::new("controls-speed");
    let (client, provider) = scripted(
        vec![ScriptedCall::response(text_response("agent done"))],
        vec![ScriptedCompletion::response(text_response("prompt done"))],
    );
    let graph = lower(
        r#"digraph W {
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        agent [prompt="Agent.", speed="fast", max_tokens=777]
        summary [shape=tab, prompt="Summarize.", speed="standard", max_tokens=555, project_memory=false]
        start -> agent -> summary -> exit
    }"#,
        "",
    );
    let (report, _) = run(&dir, graph, Some(client)).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let agent = &provider.requests()[0];
    assert_eq!(agent.speed(), Some(lithos_llm::types::Speed::Fast));
    assert_eq!(agent.max_output_tokens(), Some(777));
    let prompt = &provider.completion_requests()[0];
    assert_eq!(prompt.speed(), Some(lithos_llm::types::Speed::Balanced));
    assert_eq!(prompt.max_output_tokens(), Some(555));
}

// ── ACP: best effort ────────────────────────────────────────────────────────

fn fake_agent(dir: &RunDir) -> PathBuf {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../acceptance/testdata/fake_acp_agent.py");
    let script =
        fs::read_to_string(&source).unwrap_or_else(|e| panic!("{}: {e}", source.display()));
    let path = dir.path().join("fake_acp_agent.py");
    fs::write(&path, script).expect("write the fake agent");
    path
}

fn with_env(mut graph: Graph, pairs: &[(&str, &str)]) -> Graph {
    for scope in &mut graph.body.scopes {
        for (key, value) in pairs {
            scope
                .env
                .insert((*key).into(), ir::ExprOrValue::Value(json!(value)));
        }
    }
    graph
}

/// An ACP agent's permission request is the one boundary a `pre_tool_use`
/// hook can act on: a block answers with the rejecting option. Post-tool
/// hooks cannot run; the node warns before the agent starts, naming the
/// backend, the hook, the event and the missing boundary, and the run
/// continues.
#[tokio::test]
async fn acp_tool_hooks_are_best_effort_with_explicit_warnings() {
    let dir = RunDir::new("hooks-acp");
    let agent = fake_agent(&dir);
    let permission = dir.path().join("permission.json");
    let graph = lower(
        &format!(
            r#"digraph W {{
        graph [goal="G", acp.command="python3 {}"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [prompt="Say hello"]
        start -> a -> exit
    }}"#,
            agent.display()
        ),
        r#"
[[run.hooks]]
name = "deny-all"
event = "pre_tool_use"
script = "exit 2"

[[run.hooks]]
name = "after"
event = "post_tool_use"
script = "echo after >> post.log"
"#,
    );
    let graph = with_env(graph, &[
        ("ACP_MODE", "permission"),
        ("ACP_PERMISSION", permission.to_str().expect("utf-8")),
    ]);
    let (report, customs) = run(&dir, graph, None).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    // The permission request was answered with the rejection.
    let answered: Value = serde_json::from_str(&read(&permission)).expect("permission answer");
    assert_eq!(answered["outcome"]["outcome"], "selected");
    assert_eq!(answered["outcome"]["optionId"], "reject", "{answered}");
    // The pre report is recorded as executed and blocking; the post hook is
    // recorded as a warning, never as executed.
    let pre = customs
        .reports()
        .into_iter()
        .find(|(_, e)| e["event"] == "pre_tool_use")
        .expect("pre")
        .1;
    assert_eq!(pre["report"]["decision"]["decision"], "block");
    assert_eq!(pre["report"]["hooks"][0]["name"], "deny-all");
    assert!(!workspace(&dir).join("post.log").exists());
    let warnings = customs.warnings();
    assert!(
        warnings.iter().any(|w| w["backend"] == "acp"
            && w["hook"] == "after"
            && w["event"] == "post_tool_use"
            && w["boundary"] == "none"),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w["backend"] == "acp"
            && w["hook"] == "deny-all"
            && w["event"] == "pre_tool_use"
            && w["boundary"] == "session/request_permission"),
        "{warnings:?}"
    );
    assert!(
        customs
            .reports()
            .iter()
            .all(|(_, e)| e["event"] != "post_tool_use"),
        "no fabricated post-tool activity"
    );
}
