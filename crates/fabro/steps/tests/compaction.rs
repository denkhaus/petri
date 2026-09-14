//! Agent context compaction, readiness item 9e, against a scripted model:
//! the trigger at Fabro's threshold, continuation after the summary, later
//! thread reuse, a host summary policy, a failed summary, cancellation
//! during the summary call, the lost-thread fallback, and the public events a
//! consumer accounts from.
//!
//! The scripted catalog's `test/model` has a 200,000-token window, so Fabro's
//! 80 percent trigger is 160,000 tokens: Pebble compacts when the last
//! reported usage plus the turns since exceeds it, checked before and after
//! each model turn. Four tool rounds and a final answer make ten turns, of
//! which the six most recent stay verbatim; the safe cut keeps the second
//! tool call with its result, so the first exchange is what gets summarized.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use execution::events::{RunEvent, replay_run_dir};
use execution::host;
use fabro_steps::agent::THREAD_EVENT;
use fabro_steps::compaction::{CompactionPolicyHandle, EVENT};
use fabro_steps::pebble::PebbleClient;
use fabro_steps::register;
use frontend::{CompileInputs, NoFiles};
use ir::{CancelScopeId, Graph, RunStatus, StepEvent, Value};
use lithos_llm::types::{ErrorKind, Role, TokenCounts};
use pebble_agent::LifecycleError;
use pebble_coding_agent::events::TokenUsage;
use pebble_coding_agent::extensions::{CompactionPolicy, CompactionPreparation, CompactionSummary};
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedCompletion, ScriptedFailure, ScriptedProvider, client_from, message_text,
    text_response, tool_call_response, with_usage,
};
use runtime::driver::{EventObserver, ExecutionReport};
use runtime::engine::{EngineState, Event, EventRecord};
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, backend_event, output_of, status_of};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// The scripted model's window, from Pebble's test catalog.
const WINDOW: u64 = 200_000;
/// Fabro's trigger: strictly above 80 percent of the window.
const THRESHOLD: u64 = WINDOW * 80 / 100;

/// Every `StepEvent::Custom` the run emitted, with its node.
#[derive(Default)]
struct Customs(Mutex<Vec<(String, Value)>>);

impl EventObserver for Customs {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, state: &EngineState) {
        if let Event::StepProgressRecorded {
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

    /// Pebble's own compaction events, by variant name, in order.
    fn pebble_compaction_events(&self) -> Vec<(String, String)> {
        self.all()
            .into_iter()
            .filter(|(_, v)| v["kind"] == "pebble")
            .filter_map(|(node, v)| {
                let event = v["event"]["event"].as_object()?;
                let name = event.keys().next()?;
                name.starts_with("Compaction").then(|| (node, name.clone()))
            })
            .collect()
    }

    /// The `context_window` warnings Pebble emitted.
    fn window_warnings(&self) -> Vec<Value> {
        self.all()
            .into_iter()
            .filter(|(_, v)| v["kind"] == "pebble")
            .filter_map(|(_, v)| {
                let warning = v["event"]["event"].get("Warning")?;
                (warning["kind"] == "context_window").then(|| warning.clone())
            })
            .collect()
    }

    /// Petri's `fabro.compaction` events, `(node, payload)`.
    fn compactions(&self) -> Vec<(String, Value)> {
        self.all()
            .into_iter()
            .filter(|(_, v)| v["kind"] == EVENT)
            .collect()
    }

    fn threads(&self) -> Vec<(String, Value)> {
        self.all()
            .into_iter()
            .filter(|(_, v)| v["kind"] == THREAD_EVENT)
            .collect()
    }
}

/// Wake a waiter when Pebble reports that a compaction started.
struct CompactionStarted(mpsc::Sender<()>);
impl EventObserver for CompactionStarted {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _: &EngineState) {
        if let Event::StepProgressRecorded {
            ev: StepEvent::Custom(value),
            ..
        } = &record.event
            && value["kind"] == "pebble"
            && value["event"]["event"].get("CompactionStarted").is_some()
        {
            let _ = self.0.try_send(());
        }
    }
}

fn lower(dot: &str) -> Graph {
    let lowered = frontend_fabro::load("wf/w.fabro", dot, &NoFiles, &CompileInputs::new());
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        lowered.diagnostics
    );
    lowered.graph.expect("lowers")
}

/// One agent node `a` on the scripted model.
fn one_node(extra: &str) -> Graph {
    lower(&format!(
        r#"digraph W {{
        graph [backend="api", default_model="test/model", goal="Ship it"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [prompt="Do the work." {extra}]
        start -> a -> exit
    }}"#
    ))
}

/// Two `full` nodes on one thread.
fn two_nodes_on_a_thread(extra_a: &str) -> Graph {
    lower(&format!(
        r#"digraph W {{
        graph [backend="api", default_model="test/model", goal="Ship it", default_fidelity="full", default_thread="t"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [prompt="Do the work." {extra_a}]
        b [prompt="Finish the work."]
        start -> a -> b -> exit
    }}"#
    ))
}

fn scripted(
    stream: Vec<ScriptedCall>,
    completions: Vec<ScriptedCompletion>,
) -> (lithos_llm::Client, Arc<ScriptedProvider>) {
    client_from(ScriptedProvider::new(stream).completing(completions))
}

fn runtime(dir: &RunDir, client: lithos_llm::Client) -> (Runtime, Arc<Customs>) {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Always;
    options.echo = false;
    let customs = Arc::new(Customs::default());
    let rt = register(
        Runtime::standard()
            .observe(customs.clone())
            .options(options)
            .capability(PebbleClient(client)),
    );
    (rt, customs)
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

fn metrics(report: &ExecutionReport, node: &str) -> Value {
    json!(
        report
            .state
            .history()
            .iter()
            .find(|row| row.name == node)
            .unwrap_or_else(|| panic!("{node} finished"))
            .outcome
            .metrics
            .custom
    )
}

fn usage(input: u64, output: u64) -> TokenCounts {
    TokenCounts {
        input,
        output,
        ..TokenCounts::default()
    }
}

fn shell(id: &str, command: &str) -> ScriptedCall {
    ScriptedCall::response(tool_call_response("shell", id, json!({"command": command})))
}

/// Four tool rounds and a final answer: the first tool's output is what a
/// compaction discards; the last response reports `total` tokens, which
/// Pebble compares with the threshold as soon as the response is committed.
fn four_rounds_then(total: u64, answer: &str) -> Vec<ScriptedCall> {
    vec![
        shell("first", "echo FIRST_OUTPUT_MARKER"),
        shell("second", "echo second"),
        shell("third", "echo third"),
        shell("fourth", "echo fourth"),
        ScriptedCall::response(with_usage(text_response(answer), usage(total - 5, 5))),
    ]
}

fn summary(text: &str) -> ScriptedCompletion {
    ScriptedCompletion::response(with_usage(text_response(text), usage(70, 7)))
}

/// A request as the provider saw it, serialized: tool results are parts,
/// not text, so a marker search needs the whole shape.
fn request_text(provider: &ScriptedProvider, index: usize) -> String {
    serde_json::to_string(&provider.requests()[index]).expect("a request serializes")
}

/// Which markers each streaming request carried, for failure messages.
fn marker_map(provider: &ScriptedProvider) -> Vec<(usize, Vec<&'static str>)> {
    let markers = [
        "FIRST_OUTPUT_MARKER",
        "FIFTH_DONE",
        "SUMMARY OF THE FIRST EXCHANGE",
        "HOST SUMMARY",
        "A different assistant began",
        "Do the work.",
        "Finish the work.",
    ];
    (0..provider.requests().len())
        .map(|index| {
            let text = request_text(provider, index);
            (
                index,
                markers
                    .iter()
                    .copied()
                    .filter(|marker| text.contains(marker))
                    .collect(),
            )
        })
        .collect()
}

/// The system messages of a request, joined.
fn system_text(provider: &ScriptedProvider, index: usize) -> String {
    provider.requests()[index]
        .messages()
        .iter()
        .filter(|m| m.role() == Role::System)
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fabro's trigger is strictly above 80 percent of the window: a history
/// reported one token below, and exactly at, the threshold is left alone;
/// one token above is compacted, with Pebble's warning first.
#[tokio::test]
async fn the_trigger_is_strictly_above_eighty_percent_of_the_window() {
    for (label, total, compacted) in [
        ("below", THRESHOLD - 1, false),
        ("at", THRESHOLD, false),
        ("above", THRESHOLD + 1, true),
    ] {
        let dir = RunDir::new(&format!("compaction-trigger-{label}"));
        let (client, provider) = scripted(four_rounds_then(total, "done"), vec![summary(
            "SUMMARY OF THE FIRST EXCHANGE",
        )]);
        let (report, customs) = run(&dir, one_node(""), client).await;
        assert_eq!(
            report.status,
            RunStatus::Success,
            "{label}: {:?}",
            report.state.errors()
        );
        assert_eq!(
            provider.completion_count(),
            usize::from(compacted),
            "{label}: summary calls"
        );
        let events = customs.pebble_compaction_events();
        let warnings = customs.window_warnings();
        let metrics = metrics(&report, "a");
        if compacted {
            assert_eq!(events, [
                ("a".to_owned(), "CompactionStarted".to_owned()),
                ("a".to_owned(), "CompactionCompleted".to_owned())
            ]);
            assert_eq!(warnings.len(), 1, "{label}: {warnings:?}");
            assert_eq!(warnings[0]["details"]["estimated_tokens"], json!(total));
            assert_eq!(warnings[0]["details"]["context_window_size"], json!(WINDOW));
            assert_eq!(
                warnings[0]["details"]["estimate_method"],
                "api_usage_plus_local_delta"
            );
            let compactions = customs.compactions();
            assert_eq!(compactions.len(), 1, "{compactions:?}");
            let (node, payload) = &compactions[0];
            assert_eq!(node, "a");
            assert_eq!(payload["reason"], "threshold");
            assert_eq!(payload["estimated_tokens_before"], json!(total));
            assert_eq!(payload["original_turn_count"], 10);
            assert_eq!(payload["preserved_turn_count"], 7);
            assert_eq!(payload["usage"]["input"], 70);
            assert_eq!(payload["usage"]["output"], 7);
            assert_eq!(metrics["pebble.compactions"], 1);
            assert_eq!(metrics["pebble.compaction_usage"]["input"], 70);
        } else {
            assert!(events.is_empty(), "{label}: {events:?}");
            assert!(warnings.is_empty(), "{label}: {warnings:?}");
            assert!(customs.compactions().is_empty());
            assert_eq!(metrics["pebble.compactions"], 0);
            assert_eq!(metrics["pebble.compaction_usage"]["input"], 0);
        }
        // The prompt's own usage: five responses, the last one large, plus
        // the summary call, which Pebble bills to the prompt that compacted.
        let summary_input = if compacted { 70 } else { 0 };
        let summary_output = if compacted { 7 } else { 0 };
        assert_eq!(
            metrics["pebble.usage"]["input"],
            json!(4 * 10 + total - 5 + summary_input)
        );
        assert_eq!(
            metrics["pebble.usage"]["output"],
            json!(25 + summary_output)
        );
    }
}

/// A tool call whose response crosses the threshold is answered after the
/// compaction: the tool still runs, its result is paired with its call, the
/// next request carries the summary in place of the first exchange, the
/// agent finishes the work, and a later `full` node on the thread continues
/// the compacted conversation.
#[tokio::test]
async fn work_continues_after_compaction_and_a_later_node_reuses_the_thread() {
    let dir = RunDir::new("compaction-continue");
    let (client, provider) = scripted(
        vec![
            shell("first", "echo FIRST_OUTPUT_MARKER"),
            shell("second", "echo second"),
            shell("third", "echo third"),
            shell("fourth", "echo fourth"),
            // The fifth call's response crosses the threshold while its tool
            // call is still unanswered.
            ScriptedCall::response(with_usage(
                tool_call_response(
                    "shell",
                    "fifth",
                    json!({"command": "printf after > after.txt && echo FIFTH_DONE"}),
                ),
                usage(THRESHOLD, 5),
            )),
            ScriptedCall::response(text_response("A: finished")),
            ScriptedCall::response(text_response("B: finished")),
        ],
        vec![summary("SUMMARY OF THE FIRST EXCHANGE")],
    );
    let (report, customs) = run(&dir, two_nodes_on_a_thread(""), client).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "A: finished");
    assert_eq!(output_of(&report, "b")["text"], "B: finished");
    assert_eq!(
        fs::read_to_string(workspace(&dir).join("after.txt")).expect("after.txt"),
        "after",
        "the tool call the compaction interrupted still ran"
    );
    assert_eq!(provider.completion_count(), 1);
    let requests = provider.requests();
    assert_eq!(requests.len(), 7, "six for a, one for b");
    // The summary call saw the discarded exchange.
    let summary_request = &provider.completion_requests()[0];
    let summary_text = summary_request
        .messages()
        .iter()
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        summary_text.contains("handoff document") && summary_text.contains("FIRST_OUTPUT_MARKER"),
        "{summary_text}"
    );
    assert!(summary_request.tools().is_empty());
    // The request after the compaction carries the summary as a system
    // message, the interrupted call's result, and no longer the discarded
    // output.
    let after = request_text(&provider, 5);
    assert!(
        system_text(&provider, 5).contains("SUMMARY OF THE FIRST EXCHANGE"),
        "{after}"
    );
    assert!(after.contains("A different assistant began this task"));
    let map = marker_map(&provider);
    assert!(after.contains("FIFTH_DONE"), "{map:?}");
    assert!(!after.contains("FIRST_OUTPUT_MARKER"), "{map:?}");
    assert!(
        after.contains("Do the work."),
        "the task itself survives: {map:?}"
    );
    // The later node continues the compacted conversation.
    let later = request_text(&provider, 6);
    assert!(
        system_text(&provider, 6).contains("SUMMARY OF THE FIRST EXCHANGE"),
        "{later}"
    );
    assert!(later.contains("A: finished") && later.contains("Finish the work."));
    assert!(!later.contains("FIRST_OUTPUT_MARKER"));
    let threads = customs.threads();
    let b = threads
        .iter()
        .find(|(n, _)| n == "b")
        .map(|(_, v)| v.clone())
        .expect("b resolved");
    assert_eq!(b["reused"], true);
    assert_eq!(b["fidelity"], "full");
    // One compaction, attributed to `a`; `b` inherited the compacted history
    // and reports none of its own.
    let compactions = customs.compactions();
    assert_eq!(compactions.len(), 1, "{compactions:?}");
    assert_eq!(compactions[0].0, "a");
    assert_eq!(compactions[0].1["original_turn_count"], 10);
    assert_eq!(compactions[0].1["preserved_turn_count"], 7);
    assert_eq!(metrics(&report, "a")["pebble.compactions"], 1);
    assert_eq!(metrics(&report, "b")["pebble.compactions"], 0);
    assert_eq!(metrics(&report, "b")["pebble.compaction_usage"]["input"], 0);
    assert_eq!(metrics(&report, "b")["pebble.usage"]["input"], 10);
}

/// A host's summary policy, recorded as a capability.
struct HostSummary {
    seen: Mutex<Vec<(usize, usize, usize)>>,
}

#[async_trait]
impl CompactionPolicy for HostSummary {
    async fn summarize(
        &self,
        context: CompactionPreparation<'_>,
        _cancel: &CancellationToken,
    ) -> Result<CompactionSummary, LifecycleError> {
        self.seen.lock().expect("not poisoned").push((
            context.messages.len(),
            context.retained_messages.len(),
            context.default_request.messages().len(),
        ));
        Ok(CompactionSummary {
            text:            "HOST SUMMARY".to_owned(),
            usage:           TokenUsage {
                input: 3,
                output: 1,
                ..TokenUsage::default()
            },
            cost_usd_micros: Some(7),
        })
    }
}

/// An embedding host supplies the summary through Pebble's policy
/// interface: Pebble makes no summary call of its own, still chooses the
/// cut and replaces the history, and the host's usage is what Petri reports.
#[tokio::test]
async fn a_host_summary_policy_replaces_the_summary_call() {
    let dir = RunDir::new("compaction-host-policy");
    // The fourth tool call crosses the threshold, so a fifth request within
    // the same node carries the summary that replaced the first exchange.
    let (client, provider) = scripted(
        vec![
            shell("first", "echo FIRST_OUTPUT_MARKER"),
            shell("second", "echo second"),
            shell("third", "echo third"),
            shell("fourth", "echo fourth"),
            ScriptedCall::response(with_usage(
                tool_call_response("shell", "fifth", json!({"command": "echo FIFTH_DONE"})),
                usage(THRESHOLD, 5),
            )),
            ScriptedCall::response(text_response("done")),
        ],
        vec![],
    );
    let policy = Arc::new(HostSummary {
        seen: Mutex::new(Vec::new()),
    });
    let (rt, customs) = runtime(&dir, client);
    let rt = rt.capability(CompactionPolicyHandle(policy.clone()));
    let report = rt
        .run(one_node(""))
        .await
        .expect("replay is byte-identical");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        provider.completion_count(),
        0,
        "Pebble made no summary call"
    );
    let seen = policy.seen.lock().expect("not poisoned").clone();
    assert_eq!(
        seen.len(),
        1,
        "{seen:?} warnings {:?} map {:?}",
        customs.window_warnings(),
        marker_map(&provider)
    );
    let (summarized, retained, default_request) = seen[0];
    assert_eq!(summarized, 3, "the first exchange");
    assert_eq!(retained, 7, "Fabro keeps six turns plus the open one");
    assert!(default_request >= 2, "Pebble's default request is offered");
    let after = request_text(&provider, 5);
    assert!(
        system_text(&provider, 5).contains("HOST SUMMARY"),
        "the request after the cut hears the host's summary: {:?}",
        marker_map(&provider)
    );
    assert!(!after.contains("FIRST_OUTPUT_MARKER"));
    let compactions = customs.compactions();
    assert_eq!(compactions.len(), 1);
    assert_eq!(compactions[0].1["usage"]["input"], 3);
    assert_eq!(compactions[0].1["cost_usd_micros"], 7);
    assert_eq!(
        metrics(&report, "a")["pebble.compaction_cost_usd_micros"],
        7
    );
    // Pebble bills the host's summary to the prompt as well, so the same 7
    // micros are the prompt's only cost: the scripted responses report none.
    assert_eq!(metrics(&report, "a")["pebble.cost_usd_micros"], 7);
}

/// A summary call that fails leaves the history as it was: the node finishes
/// its work uncompacted, the failure is a Pebble event, nothing is counted.
#[tokio::test]
async fn a_failed_summary_leaves_the_agent_working_on_its_full_history() {
    let dir = RunDir::new("compaction-failed");
    let (client, provider) = scripted(
        vec![
            shell("first", "echo FIRST_OUTPUT_MARKER"),
            shell("second", "echo second"),
            shell("third", "echo third"),
            shell("fourth", "echo fourth"),
            ScriptedCall::response(with_usage(
                tool_call_response("shell", "fifth", json!({"command": "echo FIFTH_DONE"})),
                usage(THRESHOLD, 5),
            )),
            ScriptedCall::response(text_response("done anyway")),
        ],
        vec![ScriptedCompletion::Failure(ScriptedFailure::terminal(
            ErrorKind::Authentication,
            "summary refused",
        ))],
    );
    let (report, customs) = run(&dir, one_node(""), client).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "done anyway");
    assert_eq!(provider.completion_count(), 1, "one attempt, not retried");
    assert_eq!(customs.pebble_compaction_events(), [
        ("a".to_owned(), "CompactionStarted".to_owned()),
        ("a".to_owned(), "CompactionFailed".to_owned())
    ]);
    assert_eq!(provider.requests().len(), 6);
    let after = request_text(&provider, 5);
    assert!(
        after.contains("FIRST_OUTPUT_MARKER") && after.contains("FIFTH_DONE"),
        "the history is intact: {:?}",
        marker_map(&provider)
    );
    assert!(customs.compactions().is_empty());
    let metrics = metrics(&report, "a");
    assert_eq!(metrics["pebble.compactions"], 0);
    assert_eq!(
        metrics["pebble.usage"]["input"],
        json!(4 * 10 + THRESHOLD + 10)
    );
}

/// Cancelling the run while the summary call is pending cancels the
/// compaction, settles the prompt, and keeps the usage so far.
#[tokio::test]
async fn cancellation_during_the_summary_call_settles_the_session() {
    let dir = RunDir::new("compaction-cancel");
    let (client, provider) = scripted(four_rounds_then(THRESHOLD + 1, "done"), vec![
        ScriptedCompletion::Pending,
    ]);
    let (started, mut wake) = mpsc::channel(1);
    let (rt, customs) = runtime(&dir, client);
    let rt = rt.observe(Arc::new(CompactionStarted(started)));
    let driver = rt.driver(one_node(""));
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    timeout(Duration::from_secs(15), wake.recv())
        .await
        .expect("compaction starts")
        .expect("observer alive");
    assert_eq!(provider.completion_count(), 1);
    handle.cancel(CancelScopeId::ROOT).await;
    let report = timeout(Duration::from_secs(15), run)
        .await
        .expect("the cancel settles")
        .expect("run task");
    assert_ne!(report.status, RunStatus::Success);
    assert_eq!(customs.pebble_compaction_events(), [
        ("a".to_owned(), "CompactionStarted".to_owned()),
        ("a".to_owned(), "CompactionCancelled".to_owned())
    ]);
    assert!(customs.compactions().is_empty());
    let metrics = metrics(&report, "a");
    assert_eq!(metrics["pebble.compactions"], 0);
    assert_eq!(
        metrics["pebble.usage"]["input"],
        json!(4 * 10 + THRESHOLD - 4)
    );
}

/// A node whose predecessor on the thread failed after compacting starts
/// again at `summary:high`, as after any lost conversation: the compacted
/// history is gone with the session and the deterministic preamble takes
/// its place.
#[tokio::test]
async fn a_lost_thread_after_compaction_degrades_to_summary_high() {
    let dir = RunDir::new("compaction-lost-thread");
    let (client, provider) = scripted(
        {
            let mut calls = four_rounds_then(THRESHOLD + 1, "not json at all");
            calls.push(ScriptedCall::response(text_response("recovered")));
            calls
        },
        vec![summary("SUMMARY OF THE FIRST EXCHANGE")],
    );
    let (report, customs) = run(
        &dir,
        two_nodes_on_a_thread(r#", output_schema="routing", output_retries=0"#),
        client,
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "a").as_deref(), Some("failure"));
    assert_eq!(output_of(&report, "b")["text"], "recovered");
    assert_eq!(
        provider.completion_count(),
        1,
        "a compacted before it failed"
    );
    let b = customs
        .threads()
        .iter()
        .find(|(n, _)| n == "b")
        .map(|(_, v)| v.clone())
        .expect("b resolved");
    assert_eq!(b["fidelity"], "summary:high");
    assert_eq!(b["fidelity_source"], "resume");
    assert_eq!(b["reused"], false);
    let later = request_text(&provider, 5);
    assert!(
        later.contains("## Stage: a") && later.contains("Status: failed"),
        "the preamble, not the conversation: {later}"
    );
    assert!(!later.contains("SUMMARY OF THE FIRST EXCHANGE"), "{later}");
    assert_eq!(metrics(&report, "a")["pebble.compactions"], 1);
    assert_eq!(metrics(&report, "b")["pebble.compactions"], 0);
}

/// A consumer of the public event stream accounts for the compaction and
/// the session's later activity from the run dir alone: Pebble's lifecycle
/// events with their session, Petri's usage event, the metrics on the
/// attempt, and the later node's activity on the reused thread.
#[tokio::test]
async fn public_events_account_for_the_compaction_and_later_activity() {
    let dir = RunDir::new("compaction-events");
    let (client, _provider) = scripted(
        vec![
            shell("first", "echo FIRST_OUTPUT_MARKER"),
            shell("second", "echo second"),
            shell("third", "echo third"),
            shell("fourth", "echo fourth"),
            ScriptedCall::response(with_usage(
                tool_call_response("shell", "fifth", json!({"command": "echo FIFTH_DONE"})),
                usage(THRESHOLD, 5),
            )),
            ScriptedCall::response(text_response("A: finished")),
            ScriptedCall::response(text_response("B: finished")),
        ],
        vec![summary("SUMMARY OF THE FIRST EXCHANGE")],
    );
    // Under the coordinator, so the run dir is the one the public events
    // are rebuilt from.
    let (rt, _) = runtime(&dir, client);
    let report = host::run(&rt, two_nodes_on_a_thread(""))
        .await
        .expect("runs");
    assert_eq!(report.status, RunStatus::Success);

    let events: Vec<RunEvent> = replay_run_dir(dir.path())
        .await
        .expect("the run dir projects");
    let node_of = |event: &RunEvent| {
        event
            .subject
            .as_ref()
            .map(|s| s.node.name.to_string())
            .unwrap_or_default()
    };
    // Pebble's lifecycle, attributed to `a`, on one session.
    let activity: Vec<(usize, String, Option<String>, String)> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            let activity = event.custom().and_then(backend_event)?;
            if activity.backend != "pebble" {
                return None;
            }
            let name = activity.envelope["event"]
                .as_object()
                .and_then(|o| o.keys().next().cloned())
                .or_else(|| activity.envelope["event"].as_str().map(str::to_owned))?;
            Some((index, node_of(event), activity.session.clone(), name))
        })
        .collect();
    let started = activity
        .iter()
        .find(|(_, _, _, name)| name == "CompactionStarted")
        .expect("CompactionStarted");
    let completed = activity
        .iter()
        .find(|(_, _, _, name)| name == "CompactionCompleted")
        .expect("CompactionCompleted");
    assert_eq!(started.1, "a");
    assert_eq!(completed.1, "a");
    assert!(started.0 < completed.0);
    let session = started.2.clone().expect("a session");
    assert_eq!(completed.2.as_deref(), Some(session.as_str()));
    // The session kept working after the compaction: the interrupted tool
    // call completed, then the model answered.
    let later_tool = activity
        .iter()
        .find(|(index, _, s, name)| {
            *index > completed.0
                && s.as_deref() == Some(session.as_str())
                && name == "ToolCallCompleted"
        })
        .expect("a tool call completed after the compaction");
    assert_eq!(later_tool.1, "a");
    // Petri's usage event for the operation.
    let usage_event = events
        .iter()
        .enumerate()
        .find_map(|(index, event)| match event.engine() {
            Some(Event::StepProgressRecorded {
                ev: StepEvent::Custom(value),
                ..
            }) if value["kind"] == EVENT => Some((index, value.clone())),
            _ => None,
        })
        .expect("fabro.compaction");
    assert!(usage_event.0 > completed.0);
    assert_eq!(usage_event.1["node"], "a");
    assert_eq!(usage_event.1["session"], json!(session));
    assert_eq!(usage_event.1["usage"]["input"], 70);
    assert_eq!(usage_event.1["reason"], "threshold");
    // The attempt's metrics carry the totals.
    let finished_a = events
        .iter()
        .find_map(|event| match event.engine() {
            Some(Event::StepFinished { outcome, .. }) if node_of(event) == "a" => {
                Some(outcome.metrics.custom.clone())
            }
            _ => None,
        })
        .expect("a finished");
    assert_eq!(finished_a["pebble.compactions"], 1);
    assert_eq!(finished_a["pebble.compaction_usage"]["input"], 70);
    // The prompt's input includes the summary call's 70 tokens.
    assert_eq!(
        finished_a["pebble.usage"]["input"],
        json!(4 * 10 + THRESHOLD + 10 + 70)
    );
    // The later node reused the thread and reported its own activity.
    let thread_b = events
        .iter()
        .find_map(|event| match event.engine() {
            Some(Event::StepProgressRecorded {
                ev: StepEvent::Custom(value),
                ..
            }) if value["kind"] == THREAD_EVENT && value["node"] == "b" => Some(value.clone()),
            _ => None,
        })
        .expect("b's thread resolution");
    assert_eq!(thread_b["reused"], true);
    assert!(
        activity
            .iter()
            .any(|(index, node, _, name)| *index > usage_event.0
                && node == "b"
                && name == "AssistantMessage"),
        "b's activity follows: {activity:?}"
    );
}
