//! Sub-agents on the native backend (readiness item 9d): a parent delegates
//! to Pebble-built children in the same workspace, the run's tool hooks and
//! the question rule hold inside them, their events and usage are attributed
//! to the parent stage, the workflow's invocation ceiling never counts them,
//! and cancellation, thread reuse and resume behave as the reference does.
//!
//! The scripted client answers calls in order and every session of a tree
//! shares it, so each parent spawns and waits in one model turn: it then
//! makes no request until the child has finished, and the script reads in
//! delivery order.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::{env, fs, thread};

use execution::host::{self, HostRun};
use execution::inspect::inspect_run;
use fabro_steps::pebble::PebbleClient;
use fabro_steps::register;
use fabro_steps::skills::FabroHome;
use fabro_steps::subagents::METRIC;
use frontend::{CompileInputs, Lowered, MapFiles, NoFiles};
use ir::{CancelScopeId, Graph, RunStatus, StepEvent, Value};
use lithos_llm::types::{ErrorKind, Request, Response};
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedFailure, multi_tool_call_response, scripted_client, text_response,
    tool_call_response,
};
use runtime::driver::ExecutionReport;
use runtime::engine::Event;
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::json;
use smol_str::SmolStr;
use testkit::{RunDir, output_of};
use tokio::time::{sleep, timeout};

fn dot(body: &str) -> String {
    format!(
        "digraph T {{\n  graph [backend=\"api\", default_model=\"test/model\"]\n  start \
         [shape=Mdiamond]\n  exit [shape=Msquare]\n{body}\n}}"
    )
}

/// One agent node `a` with `extra` attributes.
fn one_agent(extra: &str) -> String {
    dot(&format!(
        "  a [prompt=\"Delegate the work\" {extra}]\n  start -> a -> exit"
    ))
}

#[expect(
    clippy::print_stderr,
    reason = "a graph that fails to lower explains itself in the test output"
)]
fn lower(text: &str, workflow_toml: Option<&str>) -> Lowered {
    let lowered = match workflow_toml {
        Some(toml) => {
            let files = MapFiles(BTreeMap::from([(
                "wf/workflow.toml".to_string(),
                toml.to_string(),
            )]));
            frontend_fabro::load("wf/test.fabro", text, &files, &CompileInputs::new())
        }
        None => frontend_fabro::load("test.fabro", text, &NoFiles, &CompileInputs::new()),
    };
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    assert!(lowered.graph.is_some(), "lowers");
    lowered
}

fn graph(text: &str, workflow_toml: Option<&str>) -> Graph {
    lower(text, workflow_toml).graph.expect("lowers")
}

fn runtime(dir: &Path, client: lithos_llm::Client, retention: Retention) -> Runtime {
    let mut options = RunOptions::new(dir);
    options.grace = Duration::from_millis(200);
    options.retention = retention;
    options.echo = false;
    register(Runtime::standard())
        .capability(PebbleClient(client))
        .options(options)
}

fn metrics<'a>(report: &'a ExecutionReport, node: &str) -> &'a BTreeMap<SmolStr, Value> {
    &report
        .state
        .history()
        .iter()
        .find(|row| row.name == node)
        .unwrap_or_else(|| panic!("{node} finished"))
        .outcome
        .metrics
        .custom
}

/// Every `pebble` envelope the run recorded, in order.
fn pebble_events(report: &ExecutionReport) -> Vec<Value> {
    report
        .state
        .log
        .events()
        .filter_map(|event| match event {
            Event::StepProgress {
                ev: StepEvent::Custom(value),
                ..
            } if value["kind"] == "pebble" => Some(value.clone()),
            _ => None,
        })
        .collect()
}

/// The envelopes whose Pebble event is `variant`.
fn events_of<'a>(events: &'a [Value], variant: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["event"]["event"].get(variant).is_some())
        .collect()
}

/// The workspace of a single-invocation run.
fn workspace(dir: &RunDir) -> PathBuf {
    dir.path().join("scopes/scope-0/work")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// A parent turn that spawns `tasks` and waits for all of them.
fn spawn_and_wait(tasks: &[&str]) -> Response {
    let mut calls: Vec<(&str, &str, Value)> = tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            (
                "spawn_agent",
                if i == 0 { "spawn" } else { "spawn-more" },
                json!({ "task": task }),
            )
        })
        .collect();
    calls.push(("wait", "wait", json!({})));
    multi_tool_call_response(calls)
}

/// The tool names a request advertised.
fn tool_names(request: &Request) -> Vec<String> {
    request.tools().iter().map(|t| t.name.clone()).collect()
}

/// The question tool the profile registers on the root: the one whose name
/// says it asks a person.
fn question_tool(names: &[String]) -> Option<&String> {
    names.iter().find(|n| {
        let lower = n.to_ascii_lowercase();
        lower.contains("question") || lower.contains("user_input")
    })
}

// ── Delegation, attribution, accounting ─────────────────────────────────────

/// A parent hands a file change to a child; the child does it in the
/// parent's workspace; the parent reports the child's answer. The events of
/// the child carry its own session and the parent's; the stage's metrics
/// account for the child from those events.
#[tokio::test]
async fn a_child_changes_the_parents_workspace_and_the_stage_accounts_for_it() {
    let dir = RunDir::new("subagents-delegate");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(spawn_and_wait(&["child: write child.txt"])),
        ScriptedCall::response(tool_call_response(
            "shell",
            "write",
            json!({"command": "printf 'from child\\n' > child.txt && echo WROTE"}),
        )),
        ScriptedCall::response(text_response("Wrote child.txt.")),
        ScriptedCall::response(text_response("The child wrote the file.")),
    ]);
    let report = runtime(dir.path(), client, Retention::Always)
        .run(graph(&one_agent(""), None))
        .await
        .expect("replay");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        read(&workspace(&dir).join("child.txt")),
        "from child\n",
        "the child acted in the parent's workspace"
    );
    assert_eq!(output_of(&report, "a")["text"], "The child wrote the file.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let wait_result = serde_json::to_string(&requests[3]).expect("request");
    assert!(
        wait_result.contains("Agent completed (success: true")
            && wait_result.contains("Wrote child.txt."),
        "the parent saw the child's result: {wait_result}"
    );

    // Events: the lifecycle under the parent's session, the child's own
    // events under its session naming the parent, one stream and sequence.
    let events = pebble_events(&report);
    let parent_session = events[0]["event"]["session_id"]
        .as_str()
        .expect("session")
        .to_owned();
    let spawned = events_of(&events, "SubAgentSpawned");
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0]["event"]["session_id"], parent_session);
    assert_eq!(spawned[0]["event"]["event"]["SubAgentSpawned"]["depth"], 1);
    assert_eq!(
        spawned[0]["event"]["event"]["SubAgentSpawned"]["task"],
        "child: write child.txt"
    );
    let completed = events_of(&events, "SubAgentCompleted");
    assert_eq!(completed.len(), 1);
    assert_eq!(
        completed[0]["event"]["event"]["SubAgentCompleted"]["success"],
        true
    );
    assert_eq!(
        events_of(&events, "SubAgentClosed").len(),
        1,
        "shutdown closed the child"
    );
    let child_events: Vec<&Value> = events
        .iter()
        .filter(|e| e["event"]["parent_session_id"] == parent_session)
        .collect();
    assert!(!child_events.is_empty());
    let child_session = child_events[0]["event"]["session_id"]
        .as_str()
        .expect("child session");
    assert_ne!(child_session, parent_session);
    assert!(
        child_events
            .iter()
            .any(|e| e["event"]["event"].get("ToolCallCompleted").is_some()),
        "the child's tool call is on the stream"
    );
    for (index, envelope) in events.iter().enumerate() {
        assert_eq!(envelope["node"], "a", "every event is the parent stage's");
        assert_eq!(envelope["event"]["stream_id"], parent_session);
        assert_eq!(
            envelope["event"]["seq"],
            json!(index + 1),
            "one sequence for the tree"
        );
    }

    // Accounting: the parent's own usage and the children's, reconstructed
    // from the events.
    let custom = metrics(&report, "a");
    assert_eq!(custom["pebble.usage"]["input"], 20, "two parent messages");
    let subagents = &custom[METRIC];
    assert_eq!(subagents["spawned"], 1);
    assert_eq!(subagents["completed"], 1);
    assert_eq!(subagents["failed"], 0);
    assert_eq!(subagents["closed"], 1);
    assert_eq!(subagents["usage"]["input"], 20, "two child messages");
    assert_eq!(
        subagents["sessions"][child_session]["parent"],
        parent_session
    );
    assert_eq!(subagents["sessions"][child_session]["messages"], 2);
    let mut from_events: BTreeMap<String, u64> = BTreeMap::new();
    for e in &events {
        if let (Some(message), Some(session)) = (
            e["event"]["event"].get("AssistantMessage"),
            e["event"]["session_id"].as_str(),
        ) && e["event"]["parent_session_id"].is_string()
        {
            *from_events.entry(session.to_owned()).or_default() +=
                message["usage"]["input"].as_u64().unwrap_or(0);
        }
    }
    assert_eq!(
        from_events,
        BTreeMap::from([(child_session.to_owned(), 20)])
    );
}

/// A `pre_tool_use` hook configured for the run blocks a child's tool call
/// before its effect, and the child sees the reason.
#[tokio::test]
async fn the_runs_tool_hooks_apply_inside_a_child() {
    let dir = RunDir::new("subagents-hooks");
    let ws = workspace(&dir);
    fs::create_dir_all(&ws).expect("workspace");
    fs::write(ws.join("important.txt"), "precious\n").expect("seed");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(spawn_and_wait(&["child: clean up"])),
        ScriptedCall::response(tool_call_response(
            "shell",
            "destroy",
            json!({"command": "rm -f important.txt && echo REMOVED"}),
        )),
        ScriptedCall::response(tool_call_response(
            "shell",
            "safe",
            json!({"command": "printf 'kept\\n' > safe.txt && echo SAFE"}),
        )),
        ScriptedCall::response(text_response("Cleaned up without deleting anything.")),
        ScriptedCall::response(text_response("The child cleaned up.")),
    ]);
    let toml = r#"
[[run.hooks]]
name = "no-destruction"
event = "pre_tool_use"
script = "if grep -q 'rm ' \"$FABRO_HOOK_CONTEXT\"; then echo '{\"decision\":\"block\",\"reason\":\"destructive commands are not allowed\"}'; exit 2; fi"

[[run.hooks]]
name = "log-tools"
event = "post_tool_use"
script = "echo ran:$FABRO_NODE_ID >> tool-hooks.log"
"#;
    let report = runtime(dir.path(), client, Retention::Always)
        .run(graph(&one_agent(""), Some(toml)))
        .await
        .expect("replay");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        read(&ws.join("important.txt")),
        "precious\n",
        "the blocked rm never ran"
    );
    assert_eq!(read(&ws.join("safe.txt")), "kept\n");
    let log = read(&ws.join("tool-hooks.log"));
    assert!(
        log.contains("ran:a"),
        "the post hook saw the child's call under the parent node: {log}"
    );
    // The child's first request follows the parent's spawn; which of the
    // parent's `wait` turn and the child's turn reaches the provider first
    // is a scheduling race, so the block reason is looked for in any
    // request rather than at a fixed position.
    let requests: Vec<String> = provider
        .requests()
        .iter()
        .map(|request| serde_json::to_string(request).expect("request"))
        .collect();
    assert!(
        requests
            .iter()
            .any(|request| request.contains("destructive commands are not allowed")),
        "the child saw the block reason: {requests:?}"
    );
    assert_eq!(output_of(&report, "a")["text"], "The child cleaned up.");
}

/// The root advertises the question tool; a child does not. A child does
/// carry the sub-agent tools and the workspace tools.
#[tokio::test]
async fn a_child_has_no_question_tool() {
    let dir = RunDir::new("subagents-questions");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(spawn_and_wait(&["child: look around"])),
        ScriptedCall::response(text_response("Looked.")),
        ScriptedCall::response(text_response("Done.")),
    ]);
    let report = runtime(dir.path(), client, Retention::Never)
        .run(graph(&one_agent(""), None))
        .await
        .expect("replay");
    assert_eq!(report.status, RunStatus::Success);
    let requests = provider.requests();
    let parent = tool_names(&requests[0]);
    let child = tool_names(&requests[1]);
    let question = question_tool(&parent).expect("the root has a question tool");
    assert!(
        !child.contains(question),
        "the child has no {question}: {child:?}"
    );
    for tool in ["spawn_agent", "send_input", "wait", "close_agent", "shell"] {
        assert!(
            parent.iter().any(|n| n == tool),
            "root has {tool}: {parent:?}"
        );
        assert!(
            child.iter().any(|n| n == tool),
            "child has {tool}: {child:?}"
        );
    }
}

/// A child a Fabro agent spawns re-reads the project documents; a Pebble
/// child is given none. Recorded as a library contract item; the test states
/// the reference behavior and fails at the pinned Pebble.
#[tokio::test]
#[ignore = "library gap: a Pebble child inherits no project memory (Fabro's child re-discovers \
            AGENTS.md); see task15-subagents.md"]
async fn a_child_reads_the_project_documents_its_parent_read() {
    let dir = RunDir::new("subagents-memory");
    let ws = workspace(&dir);
    fs::create_dir_all(&ws).expect("workspace");
    fs::write(ws.join("AGENTS.md"), "Always sign notes with -- petri\n").expect("memory");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(spawn_and_wait(&["child: write a note"])),
        ScriptedCall::response(text_response("Noted.")),
        ScriptedCall::response(text_response("Done.")),
    ]);
    let report = runtime(dir.path(), client, Retention::Always)
        .run(graph(&one_agent(""), None))
        .await
        .expect("replay");
    assert_eq!(report.status, RunStatus::Success);
    let requests = provider.requests();
    let parent = serde_json::to_string(&requests[0]).expect("request");
    assert!(
        parent.contains("sign notes with -- petri"),
        "the root read AGENTS.md"
    );
    let child = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        child.contains("sign notes with -- petri"),
        "the reference child reads the project documents too: {child}"
    );
}

// ── Limits ──────────────────────────────────────────────────────────────────

/// Three children at once are Pebble sessions inside one invocation: a run
/// whose ceiling admits the root alone still completes with all of them.
#[tokio::test]
async fn agent_children_do_not_consume_the_invocation_ceiling() {
    let dir = RunDir::new("subagents-ceiling");
    let (client, _) = scripted_client(vec![
        ScriptedCall::response(spawn_and_wait(&[
            "child alpha: report",
            "child beta: report",
            "child gamma: report",
        ])),
        ScriptedCall::response(text_response("Reporting.")),
        ScriptedCall::response(text_response("Reporting.")),
        ScriptedCall::response(text_response("Reporting.")),
        ScriptedCall::response(text_response("All three reported.")),
    ]);
    let lowered = lower(&one_agent(""), None);
    let mut graph = lowered.graph.expect("lowers");
    // Fabro's ceiling is 10,000; one is the smallest the coordinator takes,
    // and it leaves no room for any child invocation.
    graph.policy.max_invocations = NonZeroU32::new(1);
    let rt = runtime(dir.path(), client, Retention::Never);
    let report = host::run_configured(
        &rt,
        HostRun::new(graph).with_children(lowered.children),
        |_, _| {},
    )
    .await
    .expect("the run completes");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "All three reported.");
    let custom = metrics(&report, "a");
    assert_eq!(custom[METRIC]["spawned"], 3);
    assert_eq!(custom[METRIC]["completed"], 3);
    assert_eq!(
        custom[METRIC]["sessions"]
            .as_object()
            .map(serde_json::Map::len),
        Some(3)
    );
    assert_eq!(
        inspect_run(dir.path()).expect("inspects").invocations.len(),
        1,
        "no child is a workflow invocation"
    );
}

/// A node's open-session bound refuses a spawn the tree has no room for as
/// the tool's answer; the parent reads it and carries on.
#[tokio::test]
async fn a_spawn_over_the_open_session_bound_is_refused_and_the_stage_carries_on() {
    let dir = RunDir::new("subagents-bound");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "spawn_agent",
            "spawn",
            json!({"task": "child: anything"}),
        )),
        ScriptedCall::response(text_response("No room; did it myself.")),
    ]);
    let mut graph = graph(&one_agent(""), None);
    let node = graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == "a")
        .expect("the agent node");
    assert_eq!(
        node.step.config["subagents"]["max_open_sessions"], 4,
        "the reference default"
    );
    node.step.config["subagents"]["max_open_sessions"] = json!(1);
    let report = runtime(dir.path(), client, Retention::Never)
        .run(graph)
        .await
        .expect("replay");
    assert_eq!(report.status, RunStatus::Success);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let refusal = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        refusal.contains("Cannot spawn another agent")
            && refusal.contains("(1, counting the root)"),
        "{refusal}"
    );
    assert_eq!(metrics(&report, "a")[METRIC]["spawned"], 0);
    assert_eq!(output_of(&report, "a")["text"], "No room; did it myself.");
}

// ── Failure, nesting, cancellation ──────────────────────────────────────────

/// A child whose model fails reports the failure to the waiting parent as
/// the tool's answer; the parent's stage succeeds, as in the reference.
#[tokio::test]
async fn a_childs_failure_reaches_the_parent_without_failing_the_stage() {
    let dir = RunDir::new("subagents-failure");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(spawn_and_wait(&["child: fail please"])),
        ScriptedCall::Failure(ScriptedFailure::terminal(
            ErrorKind::Authentication,
            "the child's model fell over",
        )),
        ScriptedCall::response(text_response("The child failed.")),
    ]);
    let report = runtime(dir.path(), client, Retention::Never)
        .run(graph(&one_agent(""), None))
        .await
        .expect("replay");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "The child failed.");
    let requests = provider.requests();
    let seen = serde_json::to_string(&requests[2]).expect("request");
    assert!(
        seen.contains("fell over"),
        "the parent read the failure: {seen}"
    );
    let events = pebble_events(&report);
    let failed = events_of(&events, "SubAgentFailed");
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]["event"]["event"]["SubAgentFailed"]["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("fell over"))
    );
    let custom = metrics(&report, "a");
    assert_eq!(custom[METRIC]["failed"], 1);
    assert_eq!(custom[METRIC]["completed"], 0);
    assert_eq!(
        custom[METRIC]["usage"]["input"], 0,
        "the child committed no message"
    );
}

/// A child delegates further: the grandchild's events name the child as its
/// parent and the root as the stream, and every layer is attributed to the
/// one stage.
#[tokio::test]
async fn nested_delegation_reaches_a_grandchild_within_the_open_session_bound() {
    let dir = RunDir::new("subagents-nested");
    let (client, _) = scripted_client(vec![
        ScriptedCall::response(spawn_and_wait(&["child: delegate the write"])),
        ScriptedCall::response(spawn_and_wait(&["grandchild: write deep.txt"])),
        ScriptedCall::response(tool_call_response(
            "shell",
            "write",
            json!({"command": "printf 'deep\\n' > deep.txt && echo WROTE"}),
        )),
        ScriptedCall::response(text_response("Wrote deep.txt.")),
        ScriptedCall::response(text_response("My child wrote deep.txt.")),
        ScriptedCall::response(text_response("The grandchild wrote the file.")),
    ]);
    let report = runtime(dir.path(), client, Retention::Always)
        .run(graph(&one_agent(""), None))
        .await
        .expect("replay");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(read(&workspace(&dir).join("deep.txt")), "deep\n");
    assert_eq!(
        output_of(&report, "a")["text"],
        "The grandchild wrote the file."
    );
    let events = pebble_events(&report);
    let root = events[0]["event"]["session_id"]
        .as_str()
        .expect("root")
        .to_owned();
    let spawned = events_of(&events, "SubAgentSpawned");
    assert_eq!(spawned.len(), 2);
    assert_eq!(spawned[0]["event"]["event"]["SubAgentSpawned"]["depth"], 1);
    assert_eq!(spawned[0]["event"]["session_id"], root);
    assert_eq!(spawned[1]["event"]["event"]["SubAgentSpawned"]["depth"], 2);
    let child = spawned[1]["event"]["session_id"]
        .as_str()
        .expect("child")
        .to_owned();
    assert_ne!(child, root, "the child published the grandchild's spawn");
    assert_eq!(spawned[1]["event"]["parent_session_id"], root);
    let grandchild_tool = events
        .iter()
        .find(|e| {
            e["event"]["event"].get("ToolCallCompleted").is_some()
                && e["event"]["parent_session_id"] == child
        })
        .expect("the grandchild's tool call, naming the child as its parent");
    assert_eq!(grandchild_tool["event"]["stream_id"], root);
    assert_eq!(grandchild_tool["node"], "a");
    let custom = metrics(&report, "a");
    assert_eq!(custom[METRIC]["spawned"], 2);
    assert_eq!(custom[METRIC]["completed"], 2);
    assert_eq!(custom[METRIC]["closed"], 2);
    assert_eq!(
        custom[METRIC]["sessions"]
            .as_object()
            .map(serde_json::Map::len),
        Some(2)
    );
}

/// Cancelling the run while the parent waits closes the child, ends its
/// tool process, and the stage settles as cancelled.
#[tokio::test]
async fn cancelling_the_run_stops_every_descendant() {
    let dir = RunDir::new("subagents-cancel");
    let marker = format!("petri-subagent-cancel-{}", testkit::unique_id());
    let waiting = dir.path().join("waiting");
    let command = format!(
        "touch '{}'; while true; do sleep 0.1; done # {marker}",
        waiting.display()
    );
    let (client, _) = scripted_client(vec![
        ScriptedCall::response(spawn_and_wait(&["child: wait forever"])),
        ScriptedCall::response(tool_call_response(
            "shell",
            "block",
            json!({"command": command}),
        )),
        ScriptedCall::PendingOpen,
    ]);
    let rt = runtime(dir.path(), client, Retention::Never);
    let driver = rt.driver(graph(&one_agent(""), None));
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    timeout(Duration::from_secs(15), async {
        while !waiting.exists() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the child's tool started");
    handle.cancel(CancelScopeId::ROOT).await;
    let report = timeout(Duration::from_secs(20), run)
        .await
        .expect("cancel settles")
        .expect("run task");
    assert_ne!(report.status, RunStatus::Success);
    assert_eq!(
        testkit::status_of(&report, "a").as_deref(),
        Some("cancelled")
    );
    let events = pebble_events(&report);
    assert_eq!(events_of(&events, "SubAgentSpawned").len(), 1);
    assert_eq!(
        events_of(&events, "SubAgentClosed").len(),
        1,
        "the child was closed on the way out"
    );
    // The child's shell loop is gone: nothing carries the marker any more.
    // The plugin's kill is asynchronous to the report; give it a moment.
    for _ in 0..50 {
        if !process_running(&marker) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !process_running(&marker),
        "the child's tool process was stopped"
    );
}

fn process_running(marker: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", marker])
        .output()
        .is_ok_and(|o| o.status.success())
}

// ── Threads and resume ──────────────────────────────────────────────────────

/// A later `full` node on the thread continues the conversation with the
/// child's result in it, and can delegate again on the retained session.
#[tokio::test]
async fn a_retained_thread_keeps_a_childs_result_for_the_next_node() {
    let dir = RunDir::new("subagents-thread");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(spawn_and_wait(&["child: find the answer"])),
        ScriptedCall::response(text_response("The answer is 42.")),
        ScriptedCall::response(text_response("Found it.")),
        ScriptedCall::response(spawn_and_wait(&["child: confirm the answer"])),
        ScriptedCall::response(text_response("Confirmed: 42.")),
        ScriptedCall::response(text_response("Confirmed.")),
    ]);
    let text = dot(
        r#"  a [prompt="Find the answer", fidelity="full", thread_id="t"]
  b [prompt="Confirm the answer", fidelity="full", thread_id="t"]
  start -> a -> b -> exit"#,
    );
    let report = runtime(dir.path(), client, Retention::Never)
        .run(graph(&text, None))
        .await
        .expect("replay");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 6);
    let second_node = serde_json::to_string(&requests[3]).expect("request");
    assert!(
        second_node.contains("The answer is 42.") && second_node.contains("Confirm the answer"),
        "the retained conversation carries the child's result: {second_node}"
    );
    assert_eq!(output_of(&report, "b")["text"], "Confirmed.");
    assert_eq!(metrics(&report, "a")[METRIC]["spawned"], 1);
    assert_eq!(
        metrics(&report, "b")[METRIC]["spawned"],
        1,
        "each stage accounts for its own children"
    );
    let events = pebble_events(&report);
    let spawned = events_of(&events, "SubAgentSpawned");
    assert_eq!(spawned.len(), 2);
    assert_eq!(spawned[0]["node"], "a");
    assert_eq!(spawned[1]["node"], "b");
    assert_eq!(
        spawned[0]["event"]["session_id"], spawned[1]["event"]["session_id"],
        "one session across the thread"
    );
}

/// A run that dies while a child works resumes by running the stage again:
/// nothing of the child is restored, and the files it had written stay in
/// the workspace.
#[tokio::test]
async fn a_resumed_run_restarts_the_stage_and_keeps_an_unfinished_childs_files() {
    let dir = RunDir::new("subagents-resume");
    let gate = dir.path().join("gate");
    let command = format!(
        "printf 'partial\\n' > partial.txt; while [ ! -f '{}' ]; do sleep 0.05; done",
        gate.display()
    );
    let (client, provider) = scripted_client(vec![
        // The first run: the child writes and blocks; the run dies.
        ScriptedCall::response(spawn_and_wait(&["child: write partial.txt"])),
        ScriptedCall::response(tool_call_response(
            "shell",
            "partial",
            json!({"command": command}),
        )),
        // The resumed run: the stage starts over.
        ScriptedCall::response(spawn_and_wait(&["child: write final.txt"])),
        ScriptedCall::response(tool_call_response(
            "shell",
            "final",
            json!({"command": "printf 'final\\n' > final.txt && echo WROTE"}),
        )),
        ScriptedCall::response(text_response("Wrote final.txt.")),
        ScriptedCall::response(text_response("Finished after the restart.")),
    ]);
    let lowered = lower(&one_agent(""), None);
    let graph = lowered.graph.expect("lowers");
    let workspace = dir.path().join("scopes/invocation-0-scope-0/work");
    let rt = runtime(dir.path(), client.clone(), Retention::Always);
    let first = tokio::spawn({
        let rt = runtime(dir.path(), client, Retention::Always);
        let graph = graph.clone();
        async move { host::run_configured(&rt, HostRun::new(graph), |_, _| {}).await }
    });
    timeout(Duration::from_secs(15), async {
        while !workspace.join("partial.txt").exists() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the child wrote its file");
    assert_eq!(provider.requests().len(), 2);
    // The crash: the run's task is dropped with the child mid-tool.
    first.abort();
    let _ = first.await;
    fs::write(&gate, "go").expect("release the blocked tool");

    let resumed = host::resume(&rt).await.expect("resumes");
    assert_eq!(
        resumed.status,
        RunStatus::Success,
        "{:?}",
        resumed.state.errors()
    );
    assert_eq!(
        read(&workspace.join("partial.txt")),
        "partial\n",
        "the unfinished child's file stays"
    );
    assert_eq!(read(&workspace.join("final.txt")), "final\n");
    assert_eq!(
        output_of(&resumed, "a")["text"],
        "Finished after the restart."
    );
    assert_eq!(
        inspect_run(dir.path()).expect("inspects").invocations.len(),
        1
    );
    let events = pebble_events(&resumed);
    let spawned = events_of(&events, "SubAgentSpawned");
    assert_eq!(
        spawned.len(),
        2,
        "the first run's spawn is in the log; the restart spawned again"
    );
    assert_ne!(
        spawned[0]["event"]["session_id"], spawned[1]["event"]["session_id"],
        "the restarted stage is a new session; no child was restored"
    );
    assert_eq!(metrics(&resumed, "a")[METRIC]["spawned"], 1);
}

// ── Skills ──────────────────────────────────────────────────────────────────

/// The `repo` skills fixture as a Git repository in the run's workspace, or
/// `None` when Git is unavailable.
fn skills_repository(dir: &RunDir) -> Option<PathBuf> {
    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("target dir");
        for entry in fs::read_dir(from).expect("fixture dir") {
            let entry = entry.expect("entry");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("type").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), &target).expect("copy");
            }
        }
    }
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../acceptance/testdata/skills");
    let ws = workspace(dir);
    copy_tree(&fixtures.join("repo"), &ws);
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&ws)
        .status();
    git.is_ok_and(|s| s.success()).then_some(ws)
}

/// The parent discovers the workspace's skills (task 14's directories); a
/// child a Fabro agent spawns re-discovers them from the shared sandbox. A
/// Pebble child is given no skill directories, so the reference behavior
/// this test states fails at the pinned Pebble; recorded as a library
/// contract item.
#[tokio::test]
#[ignore = "library gap: a Pebble child inherits no skill directories (Fabro's child re-discovers \
            the skills); see task15-subagents.md"]
async fn a_child_sees_the_skills_its_parent_discovered() {
    let dir = RunDir::new("subagents-skills");
    if skills_repository(&dir).is_none() {
        return;
    }
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(spawn_and_wait(&["child: greet Ada"])),
        ScriptedCall::response(text_response("Greeted.")),
        ScriptedCall::response(text_response("Done.")),
    ]);
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Always;
    options.echo = false;
    let rt = register(
        Runtime::standard()
            .options(options)
            .capability(PebbleClient(client))
            .capability(FabroHome(dir.path().join("no-home"))),
    );
    let report = rt.run(graph(&one_agent(""), None)).await.expect("replay");
    assert_eq!(report.status, RunStatus::Success);
    let requests = provider.requests();
    let parent = serde_json::to_string(&requests[0]).expect("request");
    assert!(
        parent.contains("# Available Skills")
            && tool_names(&requests[0]).iter().any(|n| n == "use_skill"),
        "the root discovered the repository's skills"
    );
    let child = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        child.contains("# Available Skills")
            && tool_names(&requests[1]).iter().any(|n| n == "use_skill"),
        "the reference child re-discovers the skills: tools {:?}",
        tool_names(&requests[1])
    );
}
