//! Agent context compaction, readiness item 9e, through the shipped `petri`
//! binary. A native OpenAI agent's conversation grows past 80 percent of the
//! model's context window; Pebble compacts it, the agent finishes correct
//! work, and a later node on the same thread continues the compacted
//! conversation. Provider twins on loopback report the token usage that
//! drives the trigger and serve the non-streaming summary call. No Fabro, no
//! database, no live provider.
//!
//! `gpt-5.6-sol` has a 1,050,000-token context window in the built-in
//! catalog, so the trigger is 840,000 tokens. A tool response that reports
//! 900,000 input tokens crosses it; Pebble's estimate is the last reported
//! usage plus the turns since, so the next request compacts.

mod support;

use std::fs;

use serde_json::{Map, Value, json};
use support::fabro::launch::{Case, Launch};
use support::fabro::twins::{Provider, Twin, model, scenario, shell_tool, text, tool_call};

/// The token count that carries a response past `gpt-5.6-sol`'s trigger
/// (80 percent of 1,050,000).
const OVER_THRESHOLD: u64 = 900_000;

/// A scenario whose response reports `input_tokens`, so the twin drives
/// Pebble's context estimate over the trigger.
fn heavy(mut scenario: Value, input_tokens: u64) -> Value {
    scenario["script"]["usage"] = json!({ "input_tokens": input_tokens, "output_tokens": 5 });
    scenario
}

/// A scenario whose non-streaming summary call the twin answers with
/// `summary_text`. It matches Pebble's summary prompt.
fn summary_scenario(provider: Provider, namespace: &str, id: &str, summary_text: &str) -> Value {
    scenario(
        provider,
        namespace,
        id,
        model(provider),
        "Here is the conversation to summarize",
        text(summary_text),
    )
}

/// A scenario whose summary call fails, so compaction cannot complete.
fn failing_summary_scenario(provider: Provider, namespace: &str, id: &str) -> Value {
    let mut object = Map::new();
    object.insert("scenario_id".into(), json!(id));
    object.insert("namespace".into(), json!(namespace));
    object.insert(
        "matcher".into(),
        json!({
            "endpoint": provider.endpoint(),
            "model": model(provider),
            "input_contains": "Here is the conversation to summarize",
        }),
    );
    object.insert(
        "script".into(),
        json!({
            "kind": "error",
            "status": 500,
            "message": "the summarizer is unavailable",
            "error_type": "server_error",
            "code": "server_error",
        }),
    );
    Value::Object(object)
}

/// Five tool rounds that each write a file, the fifth crossing the trigger,
/// then a final answer. The scenarios match on the previous tool's output so
/// they run in order.
fn work_scenarios(provider: Provider, namespace: &str) -> Vec<Value> {
    let model = model(provider);
    let shell = shell_tool(provider);
    let step = |id: &str, matcher: &str, command: &str| {
        scenario(
            provider,
            namespace,
            id,
            model,
            matcher,
            tool_call(id, shell, json!({ "command": command })),
        )
    };
    vec![
        step("r1", "Do the work", "printf one > f1.txt && echo OUT1"),
        step("r2", "OUT1", "printf two > f2.txt && echo OUT2"),
        step("r3", "OUT2", "printf three > f3.txt && echo OUT3"),
        step("r4", "OUT3", "printf four > f4.txt && echo OUT4"),
        heavy(
            step("r5", "OUT4", "printf five > f5.txt && echo OUT5"),
            OVER_THRESHOLD,
        ),
        // After the compaction the request carries the summary and the
        // preserved tail (OUT5), not the discarded head.
        scenario(
            provider,
            namespace,
            "answer",
            model,
            "OUT5",
            text("WORK COMPLETE"),
        ),
    ]
}

fn workflow(model: &str, second_node: &str) -> String {
    format!(
        r#"digraph Compaction {{
    graph [backend="api", goal="Ship the feature", default_fidelity="full", default_thread="t"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    build [prompt="Do the work.", model="{model}", provider="openai", on_failure="exit"]
    {second_node}
    check [shape=parallelogram, script="cat f1.txt f5.txt"]
    start -> build -> review -> check -> exit
}}"#
    )
}

/// A native agent compacts once its context crosses the trigger, finishes the
/// work correctly, and a later `full` node continues the compacted thread.
#[tokio::test]
async fn an_agent_finishes_correct_work_after_compaction_and_a_later_node_reuses_the_thread() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("compaction-continue");
    let model = model(provider);
    let mut scripts = work_scenarios(provider, &case.credential);
    scripts.push(summary_scenario(
        provider,
        &case.credential,
        "summary",
        "HANDOFF: files f1..f5 written; finish the feature.",
    ));
    // The later node continues the compacted conversation and answers.
    scripts.push(scenario(
        provider,
        &case.credential,
        "review",
        model,
        "Review the work",
        text("REVIEWED"),
    ));
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &workflow(
            model,
            &format!(r#"review [prompt="Review the work.", model="{model}", provider="openai"]"#),
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
    // The work is correct: every file the agent wrote is present.
    let workspace = case.workspace();
    for (file, contents) in [("f1.txt", "one"), ("f5.txt", "five")] {
        assert_eq!(
            fs::read_to_string(workspace.join(file)).unwrap_or_default(),
            contents,
            "{file}"
        );
    }
    // The summary call was served: compaction happened.
    assert!(
        twin.consumed().contains(&"summary".to_owned()),
        "the summary call ran: {:?}",
        twin.consumed()
    );
    assert_eq!(twin.unmatched(), 0, "{:?}", twin.request_log());
    let requests = twin.requests_for(&case.credential);
    // The request after the cut carries the summary and the preserved tail,
    // and no longer the discarded first exchange.
    let after_cut = requests
        .iter()
        .map(|r| serde_json::to_string(r).unwrap_or_default())
        .find(|body| body.contains("HANDOFF: files"))
        .expect("a request carried the summary");
    assert!(after_cut.contains("OUT5"), "the preserved tail stays");
    assert!(
        !after_cut.contains("OUT1"),
        "the discarded head is gone: {after_cut}"
    );
    // The later node continued the compacted conversation: its request holds
    // the summary and the earlier answer.
    let review = requests
        .iter()
        .map(|r| serde_json::to_string(r).unwrap_or_default())
        .find(|body| body.contains("Review the work"))
        .expect("the review request");
    assert!(review.contains("HANDOFF: files"), "{review}");
    assert!(review.contains("WORK COMPLETE"), "{review}");
    assert!(!review.contains("OUT1"), "{review}");
    let context = finished.final_context();
    assert_eq!(context["response.review"], json!("REVIEWED"));
    assert_eq!(context["last_stage"], json!("review"));
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A summary call that fails is not fatal: Pebble keeps the full history and
/// the agent finishes its work.
#[tokio::test]
async fn a_failed_summary_call_does_not_fail_the_run() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("compaction-failed-summary");
    let model = model(provider);
    let mut scripts = work_scenarios(provider, &case.credential);
    scripts.push(failing_summary_scenario(
        provider,
        &case.credential,
        "summary",
    ));
    scripts.push(scenario(
        provider,
        &case.credential,
        "review",
        model,
        "Review the work",
        text("REVIEWED"),
    ));
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &workflow(
            model,
            &format!(r#"review [prompt="Review the work.", model="{model}", provider="openai"]"#),
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
    // The work still completed.
    assert_eq!(
        fs::read_to_string(case.workspace().join("f5.txt")).unwrap_or_default(),
        "five"
    );
    // The summary call was attempted and failed; the agent kept working.
    assert!(twin.consumed().contains(&"summary".to_owned()));
    let requests = twin.requests_for(&case.credential);
    // No request carries a summary, since compaction did not complete: the
    // discarded head is still there after the failed cut.
    assert!(
        requests
            .iter()
            .map(|r| serde_json::to_string(r).unwrap_or_default())
            .any(|body| body.contains("OUT1") && body.contains("OUT5")),
        "the full history survived a failed summary"
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A node whose predecessor on the thread failed after compacting starts
/// again at `summary:high`: the compacted conversation left with the failed
/// session, so the later node hears the deterministic preamble instead.
#[tokio::test]
async fn a_node_that_lost_its_conversation_starts_at_summary_high() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("compaction-lost-thread");
    let model = model(provider);
    // The build node does the full five rounds and compacts, but its final
    // response ("WORK COMPLETE") is not a routing directive, so it fails.
    let mut scripts = work_scenarios(provider, &case.credential);
    scripts.push(summary_scenario(
        provider,
        &case.credential,
        "summary",
        "HANDOFF SUMMARY",
    ));
    // The review node degrades to summary:high and recovers.
    scripts.push(scenario(
        provider,
        &case.credential,
        "review",
        model,
        "Review the work",
        text("RECOVERED"),
    ));
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Lost {{
    graph [backend="api", goal="Ship the feature", default_fidelity="full", default_thread="t"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    build [prompt="Do the work.", model="{model}", provider="openai", output_schema="routing", output_retries=0]
    review [prompt="Review the work.", model="{model}", provider="openai"]
    start -> build -> review -> exit
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
    // The build node compacted, then failed; the review node recovered on a
    // fresh conversation with the deterministic preamble.
    assert!(
        twin.consumed().contains(&"summary".to_owned()),
        "the build node compacted: {:?}",
        twin.consumed()
    );
    assert!(twin.consumed().contains(&"review".to_owned()));
    let requests = twin.requests_for(&case.credential);
    let review = requests
        .iter()
        .map(|r| serde_json::to_string(r).unwrap_or_default())
        .find(|body| body.contains("Review the work"))
        .expect("the review request");
    assert!(
        review.contains("Status: failed") || review.contains("failed"),
        "the preamble reports the failed build: {review}"
    );
    assert!(
        !review.contains("HANDOFF SUMMARY"),
        "the compacted conversation did not carry over: {review}"
    );
    let context = finished.final_context();
    assert_eq!(context["response.review"], json!("RECOVERED"));
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Cancelling the run while the summary call is in flight stops the work
/// without finishing. The fourth tool writes the marker the harness waits
/// for, the fifth response crosses the trigger, and the summary call hangs,
/// so the SIGINT lands while the run is compacting.
#[tokio::test]
async fn cancellation_during_the_summary_call_stops_the_run() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("compaction-cancel");
    let model = model(provider);
    let shell = shell_tool(provider);
    let step = |id: &str, matcher: &str, command: &str| {
        scenario(
            provider,
            &case.credential,
            id,
            model,
            matcher,
            tool_call(id, shell, json!({ "command": command })),
        )
    };
    let mut scripts = vec![
        step("r1", "Do the work", "printf one > f1.txt && echo OUT1"),
        step("r2", "OUT1", "printf two > f2.txt && echo OUT2"),
        step("r3", "OUT2", "printf three > f3.txt && echo OUT3"),
        // The fourth tool writes the marker the harness watches for.
        step("r4", "OUT3", "printf go > compacting.txt && echo OUT4"),
        heavy(
            step("r5", "OUT4", "printf five > f5.txt && echo OUT5"),
            OVER_THRESHOLD,
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model,
            "OUT5",
            text("SHOULD NOT REACH"),
        ),
    ];
    // The summary call hangs, so the run is cancelled mid-compaction.
    let mut hang = Map::new();
    hang.insert("scenario_id".into(), json!("summary"));
    hang.insert("namespace".into(), json!(case.credential));
    hang.insert(
        "matcher".into(),
        json!({
            "endpoint": provider.endpoint(),
            "model": model,
            "input_contains": "Here is the conversation to summarize",
        }),
    );
    hang.insert("script".into(), json!({ "kind": "hang" }));
    scripts.push(Value::Object(hang));
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Cancel {{
    graph [backend="api", goal="Ship the feature"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    build [prompt="Do the work.", model="{model}", provider="openai", on_failure="exit"]
    start -> build -> exit
}}"#
        ),
        None,
    );
    let launch = Launch {
        interrupt_when: Some(case.workspace().join("compacting.txt")),
        ..Launch::default()
    };
    let finished = case.run_with(&workflow, &[], launch).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("cancelled"),
        "{}",
        finished.stderr
    );
    // The run never produced its final answer.
    assert!(
        !finished.timed_out,
        "the cancel settled the run: {}",
        finished.stderr
    );
    assert!(
        !twin.consumed().contains(&"answer".to_owned()),
        "the cancelled run never answered: {:?}",
        twin.consumed()
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}
