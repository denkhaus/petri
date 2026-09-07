//! Helpers for the sub-agent cases: a twin reply that spawns children and
//! waits for them in one turn, and the public events of a finished run read
//! back through `execution::replay_run` (the same projection a host consumes
//! live), grouped the way a consumer reconstructs an agent tree.

use std::collections::BTreeMap;
use std::path::Path;

use petri::execution::events::{EventBody, RunEvent, replay_run};
use serde_json::{Map, Value, json};

/// A reply that spawns one child per task, then waits for all of them, in
/// one turn. The parent then makes no request until every child finished,
/// which keeps a shared script deterministic.
pub(crate) fn spawn_and_wait(tasks: &[&str]) -> Value {
    let mut calls: Vec<Value> = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            json!({ "id": format!("spawn-{index}"), "name": "spawn_agent", "arguments": { "task": task } })
        })
        .collect();
    calls.push(json!({ "id": "wait", "name": "wait", "arguments": {} }));
    json!({
        "kind": "success",
        "tool_calls": calls,
        "usage": { "input_tokens": 10, "output_tokens": 5 },
    })
}

/// A reply that calls one tool with the given id.
pub(crate) fn one_call(id: &str, name: &str, arguments: Value) -> Value {
    let mut call = Map::new();
    call.insert("id".into(), json!(id));
    call.insert("name".into(), json!(name));
    call.insert("arguments".into(), arguments);
    json!({
        "kind": "success",
        "tool_calls": [Value::Object(call)],
        "usage": { "input_tokens": 10, "output_tokens": 5 },
    })
}

/// A terminal provider error (one the client does not retry), repeated for
/// every matching request.
pub(crate) fn provider_error(message: &str) -> Value {
    json!({
        "kind": "error",
        "status": 400,
        "message": message,
        "error_type": "invalid_request_error",
        "code": "invalid_request",
        "sticky": true,
    })
}

/// The public events of a finished run.
pub(crate) fn public_events(run_dir: &Path) -> Vec<RunEvent> {
    replay_run(run_dir).expect("the run replays")
}

/// One agent event of the public stream, with its attribution.
#[derive(Clone, Debug)]
pub(crate) struct Activity {
    pub(crate) node:           String,
    pub(crate) session:        String,
    pub(crate) parent_session: Option<String>,
    pub(crate) stream:         Option<String>,
    pub(crate) seq:            Option<u64>,
    /// The Pebble event, `{ "<Variant>": { ... } }` or a bare variant name.
    pub(crate) event:          Value,
}

impl Activity {
    /// The variant name of the Pebble event.
    pub(crate) fn variant(&self) -> String {
        match &self.event {
            Value::String(name) => name.clone(),
            Value::Object(map) => map.keys().next().cloned().unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// The variant's payload.
    pub(crate) fn payload(&self) -> &Value {
        match &self.event {
            Value::Object(map) => map.values().next().unwrap_or(&Value::Null),
            _ => &Value::Null,
        }
    }
}

/// Every `agent_activity` event of the run, in stream order.
pub(crate) fn activities(events: &[RunEvent]) -> Vec<Activity> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::AgentActivity(activity) if activity.backend == "pebble" => Some(Activity {
                node:           event
                    .subject
                    .as_ref()
                    .map(|s| s.node.name.to_string())
                    .unwrap_or_default(),
                session:        activity.session.clone().unwrap_or_default(),
                parent_session: activity.parent_session.clone(),
                stream:         activity.stream.clone(),
                seq:            activity.stream_seq,
                event:          activity.envelope["event"].clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The activities whose Pebble event is `variant`.
pub(crate) fn of_kind<'a>(activities: &'a [Activity], variant: &str) -> Vec<&'a Activity> {
    activities
        .iter()
        .filter(|a| a.variant() == variant)
        .collect()
}

/// The `custom` metrics of `node`'s final attempt.
pub(crate) fn node_metrics(events: &[RunEvent], node: &str) -> Value {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.body {
            EventBody::AttemptFinished {
                outcome, is_final, ..
            } if *is_final && event.subject.as_ref().is_some_and(|s| s.node.name == node) => {
                Some(json!(outcome.metrics.custom))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("{node} finished an attempt"))
}

/// Input tokens per descendant session, summed from the children's own
/// committed messages: what a public consumer computes for a tree.
pub(crate) fn descendant_usage(activities: &[Activity]) -> BTreeMap<String, u64> {
    let mut usage = BTreeMap::new();
    for activity in activities {
        if activity.parent_session.is_none() || activity.variant() != "AssistantMessage" {
            continue;
        }
        *usage.entry(activity.session.clone()).or_default() +=
            activity.payload()["usage"]["input"].as_u64().unwrap_or(0);
    }
    usage
}
