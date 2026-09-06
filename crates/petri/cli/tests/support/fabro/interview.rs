//! Interview scripts for `petri run --interview-script`, in the format
//! `cli::answer` documents.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// One script entry that answers a node's question once.
pub(crate) fn entry(id: &str, node: &str, action: Value) -> Value {
    let mut entry = Map::new();
    entry.insert("id".into(), json!(id));
    entry.insert("match".into(), json!({ "node": node }));
    entry.insert("action".into(), action);
    Value::Object(entry)
}

/// An entry with a full matcher.
pub(crate) fn entry_matching(id: &str, matcher: Value, count: u32, action: Value) -> Value {
    let mut entry = Map::new();
    entry.insert("id".into(), json!(id));
    entry.insert("match".into(), matcher);
    entry.insert("count".into(), json!(count));
    entry.insert("action".into(), action);
    Value::Object(entry)
}

pub(crate) fn choice(value: &str) -> Value {
    json!({ "kind": "choice", "value": value })
}

pub(crate) fn text(value: &str) -> Value {
    json!({ "kind": "text", "value": value })
}

pub(crate) fn negative() -> Value {
    json!({ "kind": "negative" })
}

/// Write a version 1 script to `dir` and return its path.
pub(crate) fn write(dir: &Path, name: &str, entries: &[Value]) -> PathBuf {
    let path = dir.join(format!("{name}.interviews.json"));
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({ "version": 1, "entries": entries })).expect("script"),
    )
    .expect("write the interview script");
    path
}
