//! Failure injection for the twins, and the records a run leaves about its
//! routes.
//!
//! Both twins script an error with the same fields (`status`, `error_type`,
//! `code`, `message`), a hang, and a success that stops on a refusal. A
//! scenario answers one matching request and is then spent; `repeat` lets
//! the client's own retries each meet the same failure.
//!
//! What a run reports about its routes is read from the event log it names
//! on stderr (`event log: <path>`), which is what a person or a host reads
//! after the process exits: Petri's own `StepEvent::Custom` payloads (the
//! fallback plan, `attractor.fallback.plan`; the thread resolution,
//! `attractor.thread`), and Pebble's events inside the `pebble` envelope
//! (`RouteFailover`, `RouteFailoverStopped`, `SessionStarted`,
//! `AssistantMessage`, ...), each attributed to its node.

use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::twins::Provider;

/// An error reply. `code` is the provider's stable error code, what
/// `lithos-llm` classifies the failure by when the status alone does not.
pub(crate) fn error(status: u16, error_type: &str, code: &str, message: &str) -> Value {
    json!({
        "kind": "error",
        "status": status,
        "error_type": error_type,
        "code": code,
        "message": message,
    })
}

/// A reply that never comes.
pub(crate) fn hang() -> Value {
    json!({ "kind": "hang" })
}

/// A success the model ends with a refusal: on the Anthropic twin the
/// `refusal` stop reason, which `lithos-llm` reports as a content filter
/// with provider code `refusal`.
pub(crate) fn refusal(provider: Provider) -> Value {
    assert_eq!(
        provider,
        Provider::Anthropic,
        "only the Anthropic twin scripts a refusal stop"
    );
    json!({
        "kind": "success",
        "response_text": "",
        "stop_reason": "refusal",
        "stop_details": { "type": "refusal", "category": "cyber" },
        "usage": { "input_tokens": 10, "output_tokens": 1 },
    })
}

/// A scenario that answers `repeat` matching requests before it is spent.
pub(crate) fn repeated(mut scenario: Value, repeat: u32) -> Value {
    if let Some(map) = scenario.as_object_mut() {
        map.insert("repeat".into(), json!(repeat));
    }
    scenario
}

/// A scenario with a matcher on the model and the wire endpoint alone, for
/// a request whose text the case does not want to name.
pub(crate) fn any_request(
    provider: Provider,
    namespace: &str,
    id: &str,
    model: &str,
    script: Value,
) -> Value {
    let mut scenario = Map::new();
    scenario.insert("scenario_id".into(), json!(id));
    scenario.insert("namespace".into(), json!(namespace));
    scenario.insert(
        "matcher".into(),
        json!({ "endpoint": provider.endpoint(), "model": model }),
    );
    scenario.insert("script".into(), script);
    Value::Object(scenario)
}

fn event_log(run_dir: &Path) -> Value {
    let text = fs::read_to_string(run_dir.join("events.json")).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// Every `StepEvent::Custom` record of Petri's own (a `kind` other than the
/// `pebble` envelope's), in log order, read from the event log the run
/// reported.
pub(crate) fn records(run_dir: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    collect(&event_log(run_dir), &mut out, |map| {
        map.get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "pebble")
            && map.contains_key("node")
    });
    out
}

/// The payloads of one Pebble event variant attributed to `node`, in log
/// order: what the run recorded as `agent_activity`.
pub(crate) fn pebble_events(run_dir: &Path, node: &str, variant: &str) -> Vec<Value> {
    let mut envelopes = Vec::new();
    collect(&event_log(run_dir), &mut envelopes, |map| {
        map.get("kind").and_then(Value::as_str) == Some("pebble") && map["node"] == node
    });
    envelopes
        .into_iter()
        .filter_map(|envelope| envelope["event"]["event"].get(variant).cloned())
        .collect()
}

fn collect(value: &Value, out: &mut Vec<Value>, wanted: impl Fn(&Map<String, Value>) -> bool) {
    fn walk(value: &Value, out: &mut Vec<Value>, wanted: &dyn Fn(&Map<String, Value>) -> bool) {
        match value {
            Value::Object(map) => {
                if wanted(map) {
                    out.push(value.clone());
                    return;
                }
                for child in map.values() {
                    walk(child, out, wanted);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, out, wanted);
                }
            }
            _ => {}
        }
    }
    walk(value, out, &wanted);
}

/// The records of one node, by kind.
pub(crate) fn of_node<'a>(records: &'a [Value], node: &str, kind: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|r| r["node"] == node && r["kind"] == kind)
        .collect()
}

/// `provider/model` of a record that names a route.
pub(crate) fn route(record: &Value) -> String {
    format!(
        "{}/{}",
        record["provider"].as_str().unwrap_or(""),
        record["model"].as_str().unwrap_or("")
    )
}
