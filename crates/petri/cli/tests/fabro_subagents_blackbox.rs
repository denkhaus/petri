//! Readiness item 9d through the shipped binary: a native agent delegates
//! real work to Pebble-built children in its own workspace. Provider twins
//! on loopback script both the parent's `spawn_agent`/`wait` turns and the
//! children's tool calls; the workspace, the twin's request log, `petri
//! inspect` and the public event stream (`execution::replay_run`) are what
//! the cases read. No Fabro, no database, no live provider.

mod support;

use std::fs;
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use support::fabro::launch::{Case, Launch};
use support::fabro::subagents::{
    Activity, activities, descendant_usage, node_metrics, of_kind, one_call, provider_error,
    public_events, spawn_and_wait,
};
use support::fabro::twins::{Provider, Twin, model, scenario, shell_tool, text};
use tokio::process::Command;
use tokio::time::sleep;

const PROVIDER: Provider = Provider::OpenAi;

/// One native agent node whose prompt asks it to delegate; `extra` adds
/// attributes.
fn one_agent(model: &str, prompt: &str, extra: &str) -> String {
    format!(
        r#"digraph Delegation {{
    graph [backend="api", goal="Delegate the work"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="{prompt}", model="{model}", provider="openai", on_failure="exit" {extra}]
    start -> agent -> exit
}}"#
    )
}

/// A parent delegates a file change to a child, reads the child's report,
/// and answers. The workspace holds the child's file; the public events
/// carry the lifecycle under the parent's session and the child's own
/// events under its session naming the parent; the stage's accounting
/// reconstructs from those events.
#[tokio::test]
async fn a_parent_delegates_a_workspace_change_to_a_child() {
    let mut case = Case::new("subagent-delegate");
    let model = model(PROVIDER);
    let shell = shell_tool(PROVIDER);
    let scripts = vec![
        scenario(
            PROVIDER,
            &case.credential,
            "delegate",
            model,
            "Delegate writing child.txt",
            spawn_and_wait(&["child: write child.txt"]),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "child-write",
            model,
            "child: write child.txt",
            one_call(
                "write",
                shell,
                json!({ "command": "printf 'from child\\n' > child.txt && echo WROTE" }),
            ),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "child-done",
            model,
            "WROTE",
            text("Wrote child.txt."),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "synthesize",
            model,
            "Wrote child.txt.",
            text("The child wrote the file."),
        ),
    ];
    let twin = Twin::start(PROVIDER, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(&one_agent(model, "Delegate writing child.txt.", ""), None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(
        fs::read_to_string(case.workspace().join("child.txt")).expect("child.txt"),
        "from child\n",
        "the child changed the parent's workspace"
    );
    assert_eq!(twin.consumed(), [
        "delegate",
        "child-write",
        "child-done",
        "synthesize"
    ]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    let after_wait = serde_json::to_string(&requests[3]).expect("request");
    assert!(
        after_wait.contains("Agent completed (success: true")
            && after_wait.contains("Wrote child.txt."),
        "the parent read the child's result from `wait`: {after_wait}"
    );
    let context = finished.final_context();
    assert_eq!(
        context["response.agent"],
        json!("The child wrote the file.")
    );
    let inspection = finished.inspect();
    assert_eq!(
        inspection["invocations"].as_array().map(Vec::len),
        Some(1),
        "the child is a session, not an invocation"
    );

    // The public events.
    let events = public_events(&case.run_dir);
    let agent = activities(&events);
    assert!(!agent.is_empty());
    let parent_session = agent[0].session.clone();
    assert!(
        agent.iter().all(|a| a.node == "agent"),
        "every event is the stage's"
    );
    assert!(
        agent
            .iter()
            .all(|a| a.stream.as_deref() == Some(parent_session.as_str())),
        "one stream for the tree"
    );
    let seqs: Vec<u64> = agent.iter().filter_map(|a| a.seq).collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "one sequence, no gaps"
    );
    let spawned = of_kind(&agent, "SubAgentSpawned");
    assert_eq!(spawned.len(), 1);
    assert_eq!(
        spawned[0].session, parent_session,
        "the spawn is the parent's news"
    );
    assert_eq!(spawned[0].payload()["task"], "child: write child.txt");
    assert_eq!(spawned[0].payload()["depth"], 1);
    let completed = of_kind(&agent, "SubAgentCompleted");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].payload()["success"], true);
    assert_eq!(
        of_kind(&agent, "SubAgentClosed").len(),
        1,
        "closed before the session ended"
    );
    let child_session = agent
        .iter()
        .find(|a| a.parent_session.as_deref() == Some(parent_session.as_str()))
        .map(|a| a.session.clone())
        .expect("the child's events name the parent");
    assert_ne!(child_session, parent_session);
    assert!(
        agent
            .iter()
            .any(|a| a.session == child_session && a.variant() == "ToolCallCompleted"),
        "the child's shell call is on the stream"
    );
    let tool_calls = of_kind(&agent, "ToolCallStarted");
    let parent_tools: Vec<&str> = tool_calls
        .iter()
        .filter(|a| a.session == parent_session)
        .filter_map(|a| a.payload()["tool_name"].as_str())
        .collect();
    assert_eq!(parent_tools, ["spawn_agent", "wait"], "{tool_calls:?}");

    // Accounting: the stage's metric equals what the events say.
    let metrics = node_metrics(&events, "agent");
    assert_eq!(metrics["pebble.usage"]["input"], 20, "two parent messages");
    let subagents = &metrics["pebble.subagents"];
    assert_eq!(subagents["spawned"], 1);
    assert_eq!(subagents["completed"], 1);
    assert_eq!(subagents["closed"], 1);
    assert_eq!(subagents["usage"]["input"], 20, "two child messages");
    assert_eq!(
        subagents["sessions"][&child_session]["parent"],
        parent_session
    );
    let from_events = descendant_usage(&agent);
    assert_eq!(from_events.len(), 1);
    assert_eq!(from_events[&child_session], 20);
    assert_eq!(
        subagents["sessions"][&child_session]["usage"]["input"],
        from_events[&child_session]
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A configured `pre_tool_use` hook blocks a child's destructive call before
/// its effect; the child sees the reason and takes a safe route; the post
/// hook logs the child's call under the parent node.
#[tokio::test]
async fn a_hook_blocks_a_childs_tool_effect() {
    let mut case = Case::new("subagent-hook");
    let model = model(PROVIDER);
    let shell = shell_tool(PROVIDER);
    let scripts = vec![
        scenario(
            PROVIDER,
            &case.credential,
            "delegate",
            model,
            "Delegate the cleanup",
            spawn_and_wait(&["child: clean up the workspace"]),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "destroy",
            model,
            "child: clean up the workspace",
            one_call(
                "destroy",
                shell,
                json!({ "command": "rm -f important.txt && echo REMOVED" }),
            ),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "safe",
            model,
            "destructive commands are not allowed",
            one_call(
                "safe",
                shell,
                json!({ "command": "printf 'kept\\n' > safe.txt && echo SAFE_DONE" }),
            ),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "child-done",
            model,
            "SAFE_DONE",
            text("Cleaned safely."),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "synthesize",
            model,
            "Cleaned safely.",
            text("The child cleaned up without deleting anything."),
        ),
    ];
    let twin = Twin::start(PROVIDER, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Hooked {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="printf 'precious\n' > important.txt && echo prepared"]
    agent [prompt="Delegate the cleanup.", model="{model}", provider="openai", on_failure="exit"]
    start -> prepare -> agent -> exit
}}"#
        ),
        Some(
            r#"
[[run.hooks]]
name = "no-destruction"
event = "pre_tool_use"
matcher = "shell|Bash"
script = "if grep -q 'rm ' \"$FABRO_HOOK_CONTEXT\"; then echo '{\"decision\":\"block\",\"reason\":\"destructive commands are not allowed\"}'; exit 2; fi"

[[run.hooks]]
name = "log-tools"
event = "post_tool_use"
matcher = "shell|Bash"
script = "echo ran:$FABRO_NODE_ID >> tool-hooks.log"
"#,
        ),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let workspace = case.workspace();
    assert_eq!(
        fs::read_to_string(workspace.join("important.txt")).expect("important.txt"),
        "precious\n",
        "the child's blocked rm never ran"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("safe.txt")).expect("safe.txt"),
        "kept\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("tool-hooks.log")).expect("tool-hooks.log"),
        "ran:agent\n",
        "the post hook saw the child's one shell call that ran, under the parent node"
    );
    assert_eq!(twin.consumed(), [
        "delegate",
        "destroy",
        "safe",
        "child-done",
        "synthesize"
    ]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    let denial = serde_json::to_string(&requests[2]).expect("request");
    assert!(
        denial.contains("destructive commands are not allowed"),
        "the child saw the block reason: {denial}"
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A child whose provider keeps failing: the parent's `wait` returns the
/// failure as its result, the parent answers, and the run succeeds.
#[tokio::test]
async fn a_childs_failure_is_the_parents_tool_result_and_the_run_succeeds() {
    let mut case = Case::new("subagent-failure");
    let model = model(PROVIDER);
    let scripts = vec![
        scenario(
            PROVIDER,
            &case.credential,
            "delegate",
            model,
            "Delegate badly",
            spawn_and_wait(&["child: fail please"]),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "child-fails",
            model,
            "child: fail please",
            provider_error("The child's model fell over."),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "synthesize",
            model,
            "fell over",
            text("The child failed; nothing was changed."),
        ),
    ];
    let twin = Twin::start(PROVIDER, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(&one_agent(model, "Delegate badly.", ""), None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let consumed = twin.consumed();
    assert_eq!(consumed.first().map(String::as_str), Some("delegate"));
    assert_eq!(consumed.last().map(String::as_str), Some("synthesize"));
    assert!(
        consumed.iter().filter(|id| *id == "child-fails").count() >= 1,
        "{consumed:?}"
    );
    let context = finished.final_context();
    assert_eq!(
        context["response.agent"],
        json!("The child failed; nothing was changed.")
    );
    let events = public_events(&case.run_dir);
    let agent = activities(&events);
    let failed = of_kind(&agent, "SubAgentFailed");
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0].payload()["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("fell over")),
        "{:?}",
        failed[0].payload()
    );
    let metrics = node_metrics(&events, "agent");
    assert_eq!(metrics["pebble.subagents"]["failed"], 1);
    assert_eq!(metrics["pebble.subagents"]["completed"], 0);
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Two children run at the same time in one workspace (each waits for the
/// other's marker before it finishes), `wait` reports both in spawn order,
/// and the run declared one invocation.
#[tokio::test]
async fn concurrent_children_share_the_workspace_and_no_child_is_an_invocation() {
    let mut case = Case::new("subagent-concurrent");
    let model = model(PROVIDER);
    let shell = shell_tool(PROVIDER);
    let barrier = |mine: &str, other: &str| {
        json!({ "command": format!(
            "touch {mine}.started; for i in $(seq 1 100); do [ -f {other}.started ] && break; sleep 0.05; done; [ -f {other}.started ] && echo MET_{mine} || echo ALONE_{mine}"
        ) })
    };
    let scripts = vec![
        scenario(
            PROVIDER,
            &case.credential,
            "delegate",
            model,
            "Delegate twice",
            spawn_and_wait(&["child alpha: meet beta", "child beta: meet alpha"]),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "alpha-run",
            model,
            "child alpha: meet beta",
            one_call("alpha", shell, barrier("alpha", "beta")),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "beta-run",
            model,
            "child beta: meet alpha",
            one_call("beta", shell, barrier("beta", "alpha")),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "alpha-done",
            model,
            "MET_alpha",
            text("Alpha met beta."),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "beta-done",
            model,
            "MET_beta",
            text("Beta met alpha."),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "synthesize",
            model,
            "Beta met alpha.",
            text("Both children met."),
        ),
    ];
    let twin = Twin::start(PROVIDER, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(&one_agent(model, "Delegate twice.", ""), None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let workspace = case.workspace();
    assert!(workspace.join("alpha.started").exists() && workspace.join("beta.started").exists());
    let mut consumed = twin.consumed();
    assert_eq!(consumed.first().map(String::as_str), Some("delegate"));
    assert_eq!(consumed.last().map(String::as_str), Some("synthesize"));
    consumed.sort();
    assert_eq!(consumed, [
        "alpha-done",
        "alpha-run",
        "beta-done",
        "beta-run",
        "delegate",
        "synthesize"
    ]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    let after_wait = serde_json::to_string(requests.last().expect("request")).expect("request");
    let alpha = after_wait.find("Alpha met beta.").expect("alpha's report");
    let beta = after_wait.find("Beta met alpha.").expect("beta's report");
    assert!(
        alpha < beta,
        "wait reports the children in spawn order: {after_wait}"
    );
    let inspection = finished.inspect();
    assert_eq!(inspection["invocations"].as_array().map(Vec::len), Some(1));
    let events = public_events(&case.run_dir);
    let agent = activities(&events);
    assert_eq!(of_kind(&agent, "SubAgentSpawned").len(), 2);
    assert_eq!(of_kind(&agent, "SubAgentCompleted").len(), 2);
    let metrics = node_metrics(&events, "agent");
    assert_eq!(metrics["pebble.subagents"]["spawned"], 2);
    assert_eq!(
        metrics["pebble.subagents"]["sessions"]
            .as_object()
            .map(serde_json::Map::len),
        Some(2)
    );
    assert_eq!(descendant_usage(&agent).len(), 2);
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A child delegates to a grandchild; the grandchild's events name the
/// child as their parent and the root as their stream; the file it writes
/// is in the one workspace.
#[tokio::test]
async fn a_child_delegates_to_a_grandchild() {
    let mut case = Case::new("subagent-nested");
    let model = model(PROVIDER);
    let shell = shell_tool(PROVIDER);
    let scripts = vec![
        scenario(
            PROVIDER,
            &case.credential,
            "delegate",
            model,
            "Delegate deeply",
            spawn_and_wait(&["child: delegate writing deep.txt"]),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "child-delegates",
            model,
            "child: delegate writing deep.txt",
            spawn_and_wait(&["grandchild: write deep.txt"]),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "grandchild-write",
            model,
            "grandchild: write deep.txt",
            one_call(
                "write",
                shell,
                json!({ "command": "printf 'deep\\n' > deep.txt && echo WROTE_DEEP" }),
            ),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "grandchild-done",
            model,
            "WROTE_DEEP",
            text("Wrote deep.txt."),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "child-done",
            model,
            "Wrote deep.txt.",
            text("My child wrote deep.txt."),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "synthesize",
            model,
            "My child wrote deep.txt.",
            text("The grandchild wrote the file."),
        ),
    ];
    let twin = Twin::start(PROVIDER, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(&one_agent(model, "Delegate deeply.", ""), None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(
        fs::read_to_string(case.workspace().join("deep.txt")).expect("deep.txt"),
        "deep\n"
    );
    assert_eq!(twin.consumed(), [
        "delegate",
        "child-delegates",
        "grandchild-write",
        "grandchild-done",
        "child-done",
        "synthesize"
    ]);
    let events = public_events(&case.run_dir);
    let agent = activities(&events);
    let root = agent[0].session.clone();
    let spawned = of_kind(&agent, "SubAgentSpawned");
    assert_eq!(spawned.len(), 2);
    assert_eq!(spawned[0].session, root);
    assert_eq!(spawned[0].payload()["depth"], 1);
    let child = spawned[1].session.clone();
    assert_ne!(child, root, "the child announced the grandchild");
    assert_eq!(spawned[1].parent_session.as_deref(), Some(root.as_str()));
    assert_eq!(spawned[1].payload()["depth"], 2);
    let grandchild_tool = agent
        .iter()
        .find(|a| {
            a.variant() == "ToolCallCompleted"
                && a.parent_session.as_deref() == Some(child.as_str())
        })
        .expect("the grandchild's tool call names the child");
    assert_eq!(grandchild_tool.stream.as_deref(), Some(root.as_str()));
    assert_eq!(grandchild_tool.node, "agent");
    let metrics = node_metrics(&events, "agent");
    assert_eq!(metrics["pebble.subagents"]["spawned"], 2);
    assert_eq!(metrics["pebble.subagents"]["closed"], 2);
    assert_eq!(
        metrics["pebble.subagents"]["sessions"][&child]["parent"],
        root
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Interrupting the run while the parent waits on a child whose tool runs
/// forever: the child is closed, its process is gone, nothing leaks.
#[tokio::test]
async fn interrupting_the_run_stops_the_child_and_leaks_nothing() {
    let mut case = Case::new("subagent-cancel");
    let model = model(PROVIDER);
    let shell = shell_tool(PROVIDER);
    let marker = format!("petri-subagent-cancel-{}", case.credential);
    let scripts = vec![
        scenario(
            PROVIDER,
            &case.credential,
            "delegate",
            model,
            "Delegate a long task",
            spawn_and_wait(&["child: wait forever"]),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "child-blocks",
            model,
            "child: wait forever",
            one_call(
                "block",
                shell,
                json!({ "command": format!("touch waiting.txt; while true; do sleep 0.1; done # {marker}") }),
            ),
        ),
    ];
    let twin = Twin::start(PROVIDER, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(&one_agent(model, "Delegate a long task.", ""), None);
    let finished = case
        .run_with(&workflow, &[], Launch {
            interrupt_when: Some(case.workspace().join("waiting.txt")),
            ..Launch::default()
        })
        .await;
    assert!(!finished.timed_out, "the interrupt ended the run");
    assert_eq!(
        finished.status_line(),
        Some("cancelled"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["delegate", "child-blocks"]);
    let events = public_events(&case.run_dir);
    let agent = activities(&events);
    assert_eq!(of_kind(&agent, "SubAgentSpawned").len(), 1);
    assert_eq!(
        of_kind(&agent, "SubAgentClosed").len(),
        1,
        "the child was closed on the way out: {:?}",
        agent.iter().map(Activity::variant).collect::<Vec<_>>()
    );
    let mut alive = true;
    for _ in 0..50 {
        let output = Command::new("pgrep")
            .args(["-f", &marker])
            .stdin(Stdio::null())
            .output()
            .await
            .expect("pgrep runs");
        alive = output.status.success();
        if !alive {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive, "the child's shell loop was stopped");
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Two `full` nodes on one thread: the second node's request carries the
/// first node's child result, and the second node delegates again on the
/// retained session.
#[tokio::test]
async fn a_retained_thread_carries_a_childs_result_to_the_next_node() {
    let mut case = Case::new("subagent-thread");
    let model = model(PROVIDER);
    let scripts = vec![
        scenario(
            PROVIDER,
            &case.credential,
            "find",
            model,
            "Find the answer",
            spawn_and_wait(&["child: find the answer"]),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "child-finds",
            model,
            "child: find the answer",
            text("The answer is 42."),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "found",
            model,
            "The answer is 42.",
            text("Found it."),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "confirm",
            model,
            "Confirm the answer",
            spawn_and_wait(&["child: confirm 42"]),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "child-confirms",
            model,
            "child: confirm 42",
            text("Confirmed: 42."),
        ),
        scenario(
            PROVIDER,
            &case.credential,
            "confirmed",
            model,
            "Confirmed: 42.",
            text("Confirmed."),
        ),
    ];
    let twin = Twin::start(PROVIDER, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Threads {{
    graph [backend="api", goal="Answer the question"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    find [prompt="Find the answer.", model="{model}", provider="openai", fidelity="full", thread_id="t"]
    confirm [prompt="Confirm the answer.", model="{model}", provider="openai", fidelity="full", thread_id="t"]
    start -> find -> confirm -> exit
}}"#
        ),
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), [
        "find",
        "child-finds",
        "found",
        "confirm",
        "child-confirms",
        "confirmed"
    ]);
    let requests = twin.requests_for(&case.credential);
    let second_node = serde_json::to_string(&requests[3]).expect("request");
    assert!(
        second_node.contains("The answer is 42.") && second_node.contains("Confirm the answer"),
        "the retained conversation carries the first node's child result: {second_node}"
    );
    let context = finished.final_context();
    assert_eq!(context["response.confirm"], json!("Confirmed."));
    let events = public_events(&case.run_dir);
    let agent = activities(&events);
    let spawned = of_kind(&agent, "SubAgentSpawned");
    assert_eq!(spawned.len(), 2);
    assert_eq!(spawned[0].node, "find");
    assert_eq!(spawned[1].node, "confirm");
    assert_eq!(
        spawned[0].session, spawned[1].session,
        "one session across the thread"
    );
    assert_eq!(
        node_metrics(&events, "find")["pebble.subagents"]["spawned"],
        1
    );
    assert_eq!(
        node_metrics(&events, "confirm")["pebble.subagents"]["spawned"],
        1
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}
