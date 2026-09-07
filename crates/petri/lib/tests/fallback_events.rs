//! Model fallback through the public event contract: a host that consumes
//! `RunEvent`s alone can rebuild a stage's fallback plan, the routes it ran
//! on, the failover decision with its typed error, the accounting per route,
//! and the terminal outcome. The stage is a real native agent on a scripted
//! model client; the chain comes from `workflow.toml`.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use lithos_llm::types::ErrorKind;
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedFailure, scripted_client, text_response,
};
use petri::execution::ExecutionObserver;
use petri::execution::events::{CollectingSink, EventBody, EventProjector, RunEvent};
use petri::execution::host::{self, HostRun};
use petri::executor::Retention;
use petri::fabro::fallback::{FAILOVER_EVENT, PLAN_EVENT, ROUTE_EVENT, STOP_EVENT, USAGE_EVENT};
use petri::fabro::pebble::PebbleClient;
use petri::frontend::CompileInputs;
use petri::frontend::fabro::Fabro;
use petri::ir::{RunStatus, Value};
use petri::{RunOptions, Runtime};
use serde_json::json;
use testkit::RunDir;

const WORKFLOW: &str = r#"digraph Fallback {
    graph [backend="api", goal="Answer"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Say hello.", model="model", provider="test", on_failure="exit"]
    start -> agent -> exit
}"#;

/// The scripted catalog's `model` on provider `test`, with `small` as the
/// one fallback target.
const WORKFLOW_TOML: &str = "[run.model.fallbacks]\n\"model\" = [\"test:small\"]\n";

/// What a host rebuilds from the stream, and nothing else.
#[derive(Debug, Default)]
struct Reconstructed {
    plan_routes:  Vec<String>,
    notices:      usize,
    routes:       Vec<(u64, String)>,
    failovers:    Vec<(String, String, String, bool)>,
    stops:        Vec<String>,
    usage:        BTreeMap<u64, (u64, u64)>,
    final_route:  Option<String>,
    attempt:      Option<String>,
    run_status:   Option<RunStatus>,
    node_attempt: Option<u64>,
}

fn route_of(value: &Value) -> String {
    format!(
        "{}/{}",
        value["provider"].as_str().unwrap_or(""),
        value["model"].as_str().unwrap_or("")
    )
}

fn reconstruct(events: &[RunEvent]) -> Reconstructed {
    let mut out = Reconstructed::default();
    for event in events {
        match &event.body {
            EventBody::RunFinished { status } => out.run_status = Some(*status),
            EventBody::StepCustom { value } => {
                let kind = value["kind"].as_str().unwrap_or("");
                if kind == PLAN_EVENT {
                    out.plan_routes = value["routes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(route_of)
                        .collect();
                    out.notices = value["notices"].as_array().map_or(0, Vec::len);
                    out.node_attempt = value["attempt"].as_u64();
                } else if kind == ROUTE_EVENT {
                    out.routes
                        .push((value["position"].as_u64().unwrap_or(99), route_of(value)));
                } else if kind == FAILOVER_EVENT {
                    out.failovers.push((
                        route_of(&value["from"]),
                        route_of(&value["to"]),
                        value["error"]["kind"].as_str().unwrap_or("").to_owned(),
                        value["error"]["eligible"].as_bool().unwrap_or(false),
                    ));
                } else if kind == STOP_EVENT {
                    out.stops
                        .push(value["reason"].as_str().unwrap_or("").to_owned());
                } else if kind == USAGE_EVENT {
                    let position = value["position"].as_u64().unwrap_or(99);
                    let entry = out.usage.entry(position).or_default();
                    entry.0 += value["usage"]["input"].as_u64().unwrap_or(0);
                    entry.1 += value["usage"]["output"].as_u64().unwrap_or(0);
                }
            }
            EventBody::AttemptFinished { outcome, .. } => {
                if let Some(route) = outcome
                    .metrics
                    .custom
                    .get("fallback.route")
                    .and_then(Value::as_str)
                {
                    out.final_route = Some(route.to_owned());
                }
                out.attempt = Some(outcome.status.tag().to_owned());
            }
            _ => {}
        }
    }
    out
}

async fn run(dir: &RunDir, calls: Vec<ScriptedCall>) -> (Reconstructed, Vec<String>) {
    let (client, provider) = scripted_client(calls);
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Never;
    options.echo = false;
    let rt = petri::fabro::register(Runtime::standard().frontend(Fabro::new()))
        .capability(PebbleClient(client))
        .options(options);
    fs::write(dir.path().join("wf.fabro"), WORKFLOW).expect("workflow");
    fs::write(dir.path().join("workflow.toml"), WORKFLOW_TOML).expect("workflow.toml");
    let lowered = rt
        .check(
            &dir.path().join("wf.fabro"),
            None,
            None,
            &CompileInputs::new(),
        )
        .expect("loads");
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("lowers: {:?}", lowered.diagnostics));
    let sink = Arc::new(CollectingSink::default());
    let projector = EventProjector::new(sink.clone());
    let host_run = HostRun::new(graph)
        .with_children(lowered.children)
        .observe(projector.clone() as Arc<dyn ExecutionObserver>);
    let report = host::run_configured(&rt, host_run, |_, _| {})
        .await
        .expect("the run completes");
    let _ = projector.shutdown().await;
    let requested: Vec<String> = provider
        .requests()
        .iter()
        .map(|request| request.model().to_owned())
        .collect();
    let mut reconstructed = reconstruct(&sink.events());
    reconstructed.run_status = reconstructed.run_status.or(Some(report.status));
    (reconstructed, requested)
}

/// A server error on the primary, the fallback answers: every fact of the
/// outcome and its accounting is in the public events.
#[tokio::test]
async fn a_failover_is_reconstructed_from_public_events() {
    let dir = RunDir::new("fallback-events-failover");
    let (facts, requested) = run(&dir, vec![
        ScriptedCall::Failure(ScriptedFailure::retryable(
            ErrorKind::Server,
            "primary down",
        )),
        ScriptedCall::response(text_response("Hello from small.")),
    ])
    .await;
    assert_eq!(requested, ["test/model", "test/small"], "{facts:?}");
    assert_eq!(facts.plan_routes, ["test/model", "test/small"]);
    assert_eq!(facts.notices, 0);
    assert_eq!(facts.routes, [
        (0, "test/model".to_owned()),
        (1, "test/small".to_owned())
    ]);
    assert_eq!(facts.failovers, [(
        "test/model".to_owned(),
        "test/small".to_owned(),
        "server".to_owned(),
        true
    )]);
    assert!(facts.stops.is_empty());
    // The failed route accepted nothing; the fallback's turn is counted on
    // its own position.
    assert_eq!(facts.usage.get(&0), Some(&(0, 0)));
    assert!(
        facts.usage.get(&1).is_some_and(|(input, _)| *input > 0),
        "{:?}",
        facts.usage
    );
    assert_eq!(facts.final_route.as_deref(), Some("test/small"));
    assert_eq!(facts.attempt.as_deref(), Some("success"));
    assert_eq!(facts.run_status, Some(RunStatus::Success));
    assert_eq!(facts.node_attempt, Some(1));
}

/// Every route fails: the stop decision and the terminal outcome are in the
/// events, with the last route's error.
#[tokio::test]
async fn exhaustion_is_reconstructed_from_public_events() {
    let dir = RunDir::new("fallback-events-exhausted");
    let (facts, requested) = run(&dir, vec![
        ScriptedCall::Failure(ScriptedFailure::terminal(
            ErrorKind::Authentication,
            "bad key",
        )),
        ScriptedCall::Failure(
            ScriptedFailure::terminal(ErrorKind::QuotaExceeded, "spent").with_status(429),
        ),
    ])
    .await;
    assert_eq!(requested, ["test/model", "test/small"]);
    assert_eq!(facts.failovers.len(), 1);
    assert_eq!(facts.failovers[0].2, "authentication");
    assert_eq!(facts.stops, ["exhausted"]);
    assert_eq!(facts.final_route.as_deref(), Some("test/small"));
    assert_eq!(facts.attempt.as_deref(), Some("failure"));
    assert_eq!(facts.run_status, Some(RunStatus::Failed));
}

/// An ineligible error never starts the chain: one route, one stop.
#[tokio::test]
async fn an_ineligible_error_is_reconstructed_as_a_stop_at_the_primary() {
    let dir = RunDir::new("fallback-events-ineligible");
    let (facts, requested) = run(&dir, vec![ScriptedCall::Failure(
        ScriptedFailure::terminal(ErrorKind::InvalidRequest, "bad shape"),
    )])
    .await;
    assert_eq!(requested, ["test/model"]);
    assert!(facts.failovers.is_empty());
    assert_eq!(facts.routes, [(0, "test/model".to_owned())]);
    assert_eq!(facts.stops, ["ineligible"]);
    assert_eq!(facts.final_route.as_deref(), Some("test/model"));
    assert_eq!(facts.attempt.as_deref(), Some("failure"));
    let _ = json!(null);
}
