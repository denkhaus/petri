//! The tools a native session has, as one Petri event per session.
//!
//! Pebble registers a session's tools while it builds the agent (its own,
//! the MCP servers', the sub-agent tools, the host's [`crate::host_tools`])
//! and puts no event on its stream that lists them; `SessionStarted`
//! carries the provider and model alone. A host that renders the list (the
//! tools a stage's agent could call, whether or not it did) needs it once,
//! so the native backend records it as one [`EVENT`] payload per session:
//! for the node's own session once the agent is built, read off Pebble's
//! snapshot, and for each child session when Pebble reports its
//! `SessionStarted`, as the list Pebble's inheritance gives a child.
//!
//! Each tool carries the name the model calls, the description the model
//! reads, Pebble's `ToolSource` as it is (`native`, `application`, `mcp`
//! with the server and the upstream name, `skill`), and Petri's own
//! [`Category`], which is what a view groups by.

use std::collections::HashSet;

use pebble_coding_agent::tools::{RegisteredTool, ToolCategory, ToolSource, ToolSummary};
use serde::Serialize;

/// The `StepEvent::Custom` kind: `{ kind, node, firing, attempt, session,
/// tools }`, where `tools` is the session's [`Tool`] list in Pebble's
/// order (by name).
pub const EVENT: &str = "attractor.tools";

/// The names Pebble gives its question tool, in every tool vocabulary it
/// speaks: the Anthropic-style and the OpenAI-style spelling.
const QUESTION_TOOLS: &[&str] = &["AskUserQuestion", "request_user_input"];

/// Petri's classification of a session tool: where it came from and what
/// it is for, as a view groups the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// One of Pebble's own tools (files, search, shell, web, tasks), or a
    /// skill's.
    Builtin,
    /// A tool an MCP server contributed.
    Mcp,
    /// One of Pebble's sub-agent tools (`spawn_agent`, `wait`, ...).
    Subagent,
    /// A tool the embedding host registered through `HostTools`.
    Host,
    /// The tool that asks a person a question.
    Question,
}

/// One tool as the event lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Tool {
    /// The name the model calls.
    pub name:        String,
    /// The description the model reads.
    pub description: String,
    /// Pebble's source, as it reports it.
    pub source:      ToolSource,
    pub category:    Category,
    /// Whether Pebble hands the tool to a child session: its own tools and
    /// a skill's always, an MCP tool always, a host tool when the host
    /// marked it, the question tool never.
    #[serde(skip)]
    inheritable:     bool,
}

/// Petri's category for `summary`: the question tool by its name, a host
/// tool and an MCP tool by their source, a sub-agent tool by Pebble's own
/// category, everything else built in.
fn category(summary: &ToolSummary) -> Category {
    if QUESTION_TOOLS.contains(&summary.name.as_str()) {
        return Category::Question;
    }
    match summary.source {
        ToolSource::Mcp { .. } => Category::Mcp,
        ToolSource::Native | ToolSource::Skill => match summary.category {
            ToolCategory::Subagent => Category::Subagent,
            _ => Category::Builtin,
        },
        // `Application`, and a source Pebble adds later: not Pebble's own,
        // so the host's.
        _ => Category::Host,
    }
}

/// The names of the host tools Pebble would hand to a child, read before
/// the tools are handed to the builder.
pub(crate) fn inheritable_host_tools(tools: &[RegisteredTool]) -> HashSet<String> {
    tools
        .iter()
        .filter(|tool| tool.is_inheritable())
        .map(|tool| tool.definition().name.clone())
        .collect()
}

/// The session's tools as Pebble's snapshot lists them, classified.
/// `inheritable_hosts` names the host tools marked for children.
pub(crate) fn of_session(
    summaries: &[ToolSummary],
    inheritable_hosts: &HashSet<String>,
) -> Vec<Tool> {
    summaries
        .iter()
        .map(|summary| {
            let category = category(summary);
            let inheritable = match category {
                Category::Question => false,
                Category::Host => inheritable_hosts.contains(&summary.name),
                Category::Builtin | Category::Mcp | Category::Subagent => true,
            };
            Tool {
                name: summary.name.clone(),
                description: summary.description.clone(),
                source: summary.source.clone(),
                category,
                inheritable,
            }
        })
        .collect()
}

/// The tools a child session inherits from `parent`'s: Pebble's rule as
/// Petri reads it. A tool that needs a person is never given to a child,
/// a host tool only when the host marked it, everything else always.
pub(crate) fn inherited(parent: &[Tool]) -> Vec<Tool> {
    parent
        .iter()
        .filter(|tool| tool.inheritable)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(name: &str, source: ToolSource, category: ToolCategory) -> ToolSummary {
        ToolSummary {
            name: name.to_owned(),
            description: format!("{name} does a thing"),
            source,
            category,
            invoked: false,
        }
    }

    #[test]
    fn tools_are_classified_by_name_source_and_pebble_category() {
        let summaries = vec![
            summary("shell", ToolSource::Native, ToolCategory::Shell),
            summary("use_skill", ToolSource::Skill, ToolCategory::Other),
            summary("spawn_agent", ToolSource::Native, ToolCategory::Subagent),
            summary(
                "mcp__fs__read",
                ToolSource::Mcp {
                    server_name:   "fs".into(),
                    original_name: "read".into(),
                },
                ToolCategory::Other,
            ),
            summary("record_note", ToolSource::Application, ToolCategory::Other),
            summary("host_status", ToolSource::Application, ToolCategory::Other),
            summary("AskUserQuestion", ToolSource::Native, ToolCategory::Other),
            summary(
                "request_user_input",
                ToolSource::Native,
                ToolCategory::Other,
            ),
        ];
        let hosts = HashSet::from(["record_note".to_owned()]);
        let tools = of_session(&summaries, &hosts);
        let categories: Vec<Category> = tools.iter().map(|tool| tool.category).collect();
        assert_eq!(categories, [
            Category::Builtin,
            Category::Builtin,
            Category::Subagent,
            Category::Mcp,
            Category::Host,
            Category::Host,
            Category::Question,
            Category::Question,
        ]);
        let names =
            |tools: &[Tool]| -> Vec<String> { tools.iter().map(|t| t.name.clone()).collect() };
        assert_eq!(names(&inherited(&tools)), [
            "shell",
            "use_skill",
            "spawn_agent",
            "mcp__fs__read",
            "record_note"
        ]);
        let json = serde_json::to_value(&tools[3]).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!({
                "name": "mcp__fs__read",
                "description": "mcp__fs__read does a thing",
                "source": { "kind": "mcp", "server_name": "fs", "original_name": "read" },
                "category": "mcp",
            })
        );
    }
}
