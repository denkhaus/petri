//! The text a prompt or agent stage is told about earlier stages: Fabro's
//! compact preamble, and the branch results a prompted fan-in reads.

use std::fmt::Write as _;

use frontend_fabro::kinds::GOAL_CHECK_NODE;
use ir::Value;

use crate::blobs;

/// How much of a previous stage's response the compact preamble quotes.
const EXCERPT: usize = 600;

/// The compact preamble: what earlier stages left behind, from the run
/// context the engine resolved into the config's `nodes` view.
pub fn previous_stages(nodes: &Value) -> String {
    let Value::Object(nodes) = nodes else {
        return String::new();
    };
    let mut lines = Vec::new();
    for (name, record) in nodes {
        if name == "start" || name == GOAL_CHECK_NODE {
            continue;
        }
        let status = record.get("status").and_then(Value::as_str).unwrap_or("?");
        let text = record
            .pointer("/output/text")
            .or_else(|| record.pointer("/output/stdout"))
            .and_then(Value::as_str)
            .map(|t| {
                let excerpt: String = t.chars().take(EXCERPT).collect();
                if excerpt.len() < t.len() {
                    format!("{excerpt}…")
                } else {
                    excerpt
                }
            });
        match text {
            Some(text) if !text.trim().is_empty() => {
                lines.push(format!("- {name} ({status}): {}", text.trim()));
            }
            _ => lines.push(format!("- {name} ({status})")),
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("Previous stages:\n{}\n\n", lines.join("\n"))
}

/// The branch results a prompted fan-in joins, one section per branch in
/// branch order: the source node, its status, and its result. A result that
/// still holds an output reference is named as such; the step hydrates
/// references before rendering when the store is available.
pub fn branch_results(sources: &[String], results: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Parallel branch results ({} branches: {}):",
        sources.len(),
        sources.join(", ")
    );
    match results.as_array() {
        Some(items) if !items.is_empty() => {
            for (position, item) in items.iter().enumerate() {
                let id = item.get("id").and_then(Value::as_str).unwrap_or("?");
                let status = item.get("status").and_then(Value::as_str).unwrap_or("?");
                let index = item
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(position as u64);
                let _ = writeln!(out, "\n### Branch {index}: {id} ({status})");
                let body = item
                    .get("context_updates")
                    .or_else(|| item.get("output"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if blobs::holds_ref(&body) {
                    out.push_str("(result stored as an output reference)\n");
                }
                out.push_str(&serde_json::to_string_pretty(&body).unwrap_or_default());
                out.push('\n');
            }
        }
        _ => out.push_str("(no branch results)\n"),
    }
    out.push('\n');
    out
}
