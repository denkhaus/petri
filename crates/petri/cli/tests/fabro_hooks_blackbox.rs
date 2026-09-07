//! Readiness item 5 through the shipped binary: a configured `[[run.hooks]]`
//! entry blocks a real tool effect of a native agent, the post-tool hooks see
//! the calls that ran, and a retained thread carries one conversation across
//! two `full` fidelity nodes. Provider twins on loopback, real shell tools in
//! the workspace, no Fabro, no database, no live provider.

mod support;

use std::fs;

use serde_json::{Value, json};
use support::fabro::launch::Case;
use support::fabro::twins::{Provider, Twin, model, scenario, shell_tool, text, tool_call};

/// A native agent whose first tool call would delete a file; the configured
/// `pre_tool_use` hook blocks it, the model sees the reason and takes a safe
/// route, and the post hook logs the call that ran.
#[tokio::test]
async fn a_configured_hook_blocks_a_real_tool_effect_in_the_native_backend() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("hook-blocks-tool");
    let model = model(provider);
    let shell = shell_tool(provider);
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "destroy",
            model,
            "Clean up the workspace",
            tool_call(
                "destroy",
                shell,
                json!({ "command": "rm -f important.txt && echo REMOVED" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "safe",
            model,
            "destructive commands are not allowed",
            tool_call(
                "safe",
                shell,
                json!({ "command": "printf 'kept\\n' > safe.txt && echo SAFE_DONE" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model,
            "SAFE_DONE",
            text("Cleaned up without deleting anything."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Hooked {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="printf 'precious\n' > important.txt && echo prepared"]
    agent [prompt="Clean up the workspace.", model="{model}", provider="openai", on_failure="exit"]
    verify [shape=parallelogram, script="cat important.txt safe.txt tool-hooks.log"]
    start -> prepare -> agent -> verify -> exit
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
        "the blocked rm never ran"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("safe.txt")).expect("safe.txt"),
        "kept\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("tool-hooks.log")).expect("tool-hooks.log"),
        "ran:agent\n",
        "the post hook saw the one call that ran"
    );
    assert_eq!(twin.consumed(), ["destroy", "safe", "answer"]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    let after_block = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        after_block.contains("destructive commands are not allowed"),
        "the model saw the block reason: {after_block}"
    );
    let outputs = tool_outputs(&requests[1]);
    assert_eq!(
        outputs,
        ["destructive commands are not allowed"],
        "the blocked call's only result is the hook's reason: {after_block}"
    );
    let context = finished.final_context();
    assert_eq!(
        context["response.agent"],
        json!("Cleaned up without deleting anything.")
    );
    let echoed = finished.echoed();
    assert!(
        echoed
            .iter()
            .any(|(node, line)| node == "verify" && line == "precious"),
        "{echoed:?}"
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Two `full` fidelity nodes on one thread: the second request to the
/// provider carries the first node's exchange, and a `compact` node after
/// them starts a fresh conversation with a preamble instead.
#[tokio::test]
async fn full_fidelity_nodes_share_one_conversation_through_the_binary() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("threads-binary");
    let model = model(provider);
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "plan",
            model,
            "Write a plan",
            text("PLAN: add a health endpoint"),
        ),
        scenario(
            provider,
            &case.credential,
            "implement",
            model,
            "Implement the plan",
            text("IMPLEMENTED the plan"),
        ),
        scenario(
            provider,
            &case.credential,
            "review",
            model,
            "Recent stages:",
            text("REVIEWED"),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Threads {{
    graph [backend="api", goal="Ship a health endpoint"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    plan [prompt="Write a plan.", model="{model}", provider="openai", fidelity="full", thread_id="impl"]
    implement [prompt="Implement the plan.", model="{model}", provider="openai", fidelity="full", thread_id="impl"]
    review [prompt="Review the work.", model="{model}", provider="openai", fidelity="summary:low"]
    start -> plan -> implement -> review -> exit
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
    assert_eq!(twin.consumed(), ["plan", "implement", "review"]);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 3, "{requests:?}");
    let first = serde_json::to_string(&requests[0]).expect("request");
    assert!(
        !first.contains("Goal:"),
        "full fidelity sends no preamble: {first}"
    );
    let second = serde_json::to_string(&requests[1]).expect("request");
    assert!(
        second.contains("Write a plan") && second.contains("PLAN: add a health endpoint"),
        "the second node continues the first's conversation: {second}"
    );
    let third = serde_json::to_string(&requests[2]).expect("request");
    assert!(
        third.contains("Goal: Ship a health endpoint") && third.contains("- implement: succeeded"),
        "the review starts fresh with a low summary: {third}"
    );
    assert!(
        !third.contains("PLAN: add a health endpoint") || third.contains("Recent stages"),
        "{third}"
    );
    let context = finished.final_context();
    assert_eq!(context["response.review"], json!("REVIEWED"));
    assert_eq!(context["last_stage"], json!("review"));
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// The tool results an OpenAI Responses request carries back to the model.
fn tool_outputs(request: &Value) -> Vec<String> {
    request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .filter_map(|item| item["output"].as_str().map(str::to_owned))
        .collect()
}
