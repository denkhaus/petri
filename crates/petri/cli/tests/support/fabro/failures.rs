//! Failure injection for the twins, and the fallback records a run leaves.
//!
//! Both twins script an error with the same fields (`status`, `error_type`,
//! `code`, `message`), a hang, and a success that stops on a refusal. A
//! scenario answers one matching request and is then spent; `repeat` lets
//! the client's own retries each meet the same failure.
//!
//! The fallback decisions a run took are `StepEvent::Custom` payloads whose
//! `kind` starts with `fabro.fallback.`; the run writes them into the event
//! log it reports on stderr (`event log: <path>`), which is what a person or
//! a host reads after the process exits.

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

/// Every `fabro.fallback.*` record of the run, in log order, read from the
/// event log the run reported.
pub(crate) fn records(run_dir: &Path) -> Vec<Value> {
    let text = fs::read_to_string(run_dir.join("events.json")).unwrap_or_default();
    let log: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let mut out = Vec::new();
    collect(&log, &mut out);
    out
}

fn collect(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.starts_with("fabro.fallback."))
            {
                out.push(value.clone());
                return;
            }
            for child in map.values() {
                collect(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect(item, out);
            }
        }
        _ => {}
    }
}

/// The records of one node, by kind.
pub(crate) fn of_node<'a>(records: &'a [Value], node: &str, kind: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|r| r["node"] == node && r["kind"] == kind)
        .collect()
}

/// The `kind` sequence of one node's records, without the `fabro.fallback.`
/// prefix.
pub(crate) fn kinds(records: &[Value], node: &str) -> Vec<String> {
    records
        .iter()
        .filter(|r| r["node"] == node)
        .filter_map(|r| r["kind"].as_str())
        .map(|k| k.trim_start_matches("fabro.fallback.").to_owned())
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
