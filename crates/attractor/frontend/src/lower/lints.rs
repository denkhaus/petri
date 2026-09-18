//! The rules of Fabro's validator (`fabro-validate`) that no other part of
//! the lowering raises, ported as diagnostics. `crates/attractor/LINTS.md`
//! maps every Fabro rule to the Petri code that covers it; the checks here
//! are the ports that page names.

use std::collections::HashSet;
use std::sync::LazyLock;

use frontend::{Diagnostic, Diagnostics, Span};
use regex::Regex;

use super::{Kind, shape_of};
use crate::model::{Attrs, EdgeDecl, NodeDecl, Workflow};

/// DOT's reserved words, as Fabro's `reserved_keyword_node_id` lists them.
const DOT_RESERVED_KEYWORDS: &[&str] = &[
    "graph", "digraph", "subgraph", "node", "edge", "strict", "if",
];

/// Graphviz's rank directions, as Fabro's `direction_valid` lists them.
const RANK_DIRECTIONS: &[&str] = &["TB", "LR", "BT", "RL"];

/// Attributes only some kinds of node read, with the kinds that read them.
/// On every other kind the attribute is inert: accepted and read by nothing
/// (Fabro's `inert_attribute`). Attributes several kinds read (`timeout`),
/// every node resolves (`fidelity`, `retry_policy`, `max_visits`,
/// `goal_gate`) or a stylesheet may write (`model`, `provider`,
/// `reasoning_effort`, `speed`, `backend`) are not listed. A prompted fan-in
/// is a prompt node in Petri, so it reads `output_schema` and
/// `output_retries` where Fabro's table says it does not.
const HANDLER_SPECIFIC: &[(&str, &[Kind])] = &[
    ("script", &[Kind::Command]),
    ("language", &[Kind::Command]),
    ("stdin_source", &[Kind::Command]),
    ("duration", &[Kind::Wait]),
    ("max_parallel", &[Kind::Parallel]),
    ("output_retries", &[Kind::Agent, Kind::Prompt, Kind::FanIn]),
    ("output_schema", &[
        Kind::Agent,
        Kind::Prompt,
        Kind::FanIn,
        Kind::Command,
    ]),
    ("prompt", &[Kind::Agent, Kind::Prompt, Kind::FanIn]),
    ("review_target", &[Kind::Human]),
];

/// The attributes only the API backend reads (Fabro's `backend_valid`).
const API_ONLY_ATTRS: &[&str] = &[
    "model",
    "provider",
    "reasoning_effort",
    "max_tokens",
    "speed",
];

/// `cd` into an absolute path, anywhere in a script.
static CD_ABSOLUTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bcd\s+/").expect("a constant pattern compiles"));

/// Fabro's `reserved_keyword_node_id`: a node id that is a DOT keyword
/// parses here, but other DOT tools may refuse it.
pub(super) fn reserved_keyword_node_id(node: &NodeDecl, diags: &mut Diagnostics) {
    let lower = node.id.to_ascii_lowercase();
    if !DOT_RESERVED_KEYWORDS.contains(&lower.as_str()) {
        return;
    }
    diags.push(
        Diagnostic::warning(
            "attractor.reserved_keyword_node_id",
            node.span.clone(),
            format!(
                "node id `{}` is a DOT reserved word; other DOT tools may fail to parse it",
                node.id
            ),
        )
        .with_hint(format!(
            "rename `{}` to `{lower}_step` or another id that is not a keyword",
            node.id
        )),
    );
}

/// Fabro's `direction_valid`: `rankdir` is a layout attribute, but a value
/// Graphviz does not know is a typo worth a word.
pub(super) fn rankdir(attrs: &Attrs, span: &Span, diags: &mut Diagnostics) {
    let Some(direction) = attrs.text("rankdir") else {
        return;
    };
    if RANK_DIRECTIONS.contains(&direction.as_str()) {
        return;
    }
    diags.push(
        Diagnostic::warning(
            "attractor.bad_rankdir",
            attrs.span_of("rankdir", span),
            format!("`rankdir={direction}` is not a Graphviz rank direction"),
        )
        .with_hint(format!("use one of {}", RANK_DIRECTIONS.join(", "))),
    );
}

/// Whether the node's `type` or shape names a kind, so an attribute table
/// can be applied to it. An unknown type or shape is diagnosed by
/// `Ctx::kind_of`; guessing at what its author meant helps nobody.
fn kind_is_known(node: &NodeDecl) -> bool {
    match node.attrs.text("type") {
        Some(name) => Kind::from_type(&name).is_some(),
        None => Kind::from_shape(&shape_of(node)).is_some(),
    }
}

/// Fabro's `inert_attribute` and `script_prompt_conflict`: an attribute the
/// node's kind never reads is a warning; a node that sets both `script` and
/// `prompt` is an error, because no kind reads both.
pub(super) fn inert_attributes(node: &NodeDecl, kind: Kind, diags: &mut Diagnostics) {
    let conflict = node.attrs.contains("script") && node.attrs.contains("prompt");
    if conflict {
        diags.push(
            Diagnostic::error(
                "attractor.script_prompt_conflict",
                node.attrs.span_of("script", &node.span),
                format!(
                    "node `{}` sets both `script` and `prompt`; a command runs the script and \
                     an agent or prompt node reads the prompt, and no node does both",
                    node.id
                ),
            )
            .with_hint(
                "remove the attribute that is wrong, or split the node into a command node and \
                 an agent node",
            ),
        );
    }
    if !kind_is_known(node) {
        return;
    }
    for (attr, consumers) in HANDLER_SPECIFIC {
        if !node.attrs.contains(attr) || consumers.contains(&kind) {
            continue;
        }
        if conflict && matches!(*attr, "script" | "prompt") {
            continue;
        }
        let readers = consumers
            .iter()
            .map(|kind| kind.name())
            .collect::<Vec<_>>()
            .join(", ");
        diags.push(
            Diagnostic::warning(
                "attractor.inert_attribute",
                node.attrs.span_of(attr, &node.span),
                format!(
                    "`{attr}` on node `{}` ({}) has no effect; only {readers} nodes read it",
                    node.id,
                    kind.name()
                ),
            )
            .with_hint(format!(
                "remove `{attr}`, or change the node to a kind that reads it ({readers})"
            )),
        );
    }
}

/// Fabro's `for_each_contract`, the first clause: a `for_each` on anything
/// but a parallel node never fans out, which is an error and not an inert
/// attribute because the fan-out is the point of the node.
pub(super) fn for_each_requires_parallel(node: &NodeDecl, kind: Kind, diags: &mut Diagnostics) {
    if kind == Kind::Parallel || !node.attrs.contains("for_each") {
        return;
    }
    diags.push(
        Diagnostic::error(
            "attractor.for_each.not_parallel",
            node.attrs.span_of("for_each", &node.span),
            format!(
                "`for_each` on node `{}` ({}) never fans out; only a parallel node expands a list",
                node.id,
                kind.name()
            ),
        )
        .with_hint("remove `for_each`, or make the node `shape=component`"),
    );
}

/// Fabro's `all_conditional_edges`: a node whose every outgoing edge has a
/// condition has no fallback when none holds.
pub(super) fn all_conditional_edges(node: &NodeDecl, edges: &[&EdgeDecl], diags: &mut Diagnostics) {
    if edges.is_empty() || !edges.iter().all(|edge| has_condition(edge)) {
        return;
    }
    diags.push(
        Diagnostic::error(
            "attractor.all_conditional_edges",
            node.span.clone(),
            format!(
                "every outgoing edge of `{}` has a condition, so nothing is taken when none holds",
                node.id
            ),
        )
        .with_hint("add one unconditional edge as the fallback"),
    );
}

fn has_condition(edge: &EdgeDecl) -> bool {
    edge.attrs
        .text("condition")
        .is_some_and(|condition| !condition.trim().is_empty())
}

/// Fabro's `retry_target_exists`: a retry target that names no node, on a
/// node or on the graph.
pub(super) fn retry_targets(
    attrs: &Attrs,
    span: &Span,
    workflow: &Workflow,
    what: &str,
    diags: &mut Diagnostics,
) {
    for key in ["retry_target", "fallback_retry_target"] {
        let Some(target) = attrs.text(key) else {
            continue;
        };
        if workflow.node(&target).is_some() {
            continue;
        }
        diags.push(
            Diagnostic::warning(
                "attractor.retry_target_not_found",
                attrs.span_of(key, span),
                format!("`{key}=\"{target}\"` on {what} names no node"),
            )
            .with_hint(format!("declare `{target}`, or fix `{key}`")),
        );
    }
}

/// Fabro's `script_absolute_cd`: a command that changes to an absolute
/// directory leaves the workspace it was given.
pub(super) fn script_absolute_cd(node: &NodeDecl, script: &str, diags: &mut Diagnostics) {
    if !CD_ABSOLUTE.is_match(script) {
        return;
    }
    diags.push(
        Diagnostic::warning(
            "attractor.script_absolute_cd",
            node.attrs.span_of("script", &node.span),
            format!(
                "the script of command node `{}` changes to an absolute directory (`cd /...`)",
                node.id
            ),
        )
        .with_hint("use a relative path; the command already runs in the workspace"),
    );
}

/// Fabro's `backend_valid`, the ACP clauses: an agent on `backend="acp"`
/// does not read the API-only attributes. An attribute the stylesheet wrote
/// (`styled`, as `(node, property)`) is not the node's own and is skipped.
pub(super) fn acp_api_only_attributes(
    node: &NodeDecl,
    styled: &HashSet<(String, String)>,
    diags: &mut Diagnostics,
) {
    let present: Vec<&str> = API_ONLY_ATTRS
        .iter()
        .copied()
        .filter(|attr| {
            node.attrs.contains(attr) && !styled.contains(&(node.id.clone(), (*attr).to_string()))
        })
        .collect();
    let Some(first) = present.first() else {
        return;
    };
    let list = present
        .iter()
        .map(|attr| format!("`{attr}`"))
        .collect::<Vec<_>>()
        .join(", ");
    diags.push(
        Diagnostic::error(
            "attractor.acp_api_only_attributes",
            node.attrs.span_of(first, &node.span),
            format!(
                "node `{}` runs on `backend=\"acp\"`, which does not read {list}; the ACP agent \
                 chooses its own model",
                node.id
            ),
        )
        .with_hint(format!("remove {list}, or use `backend=\"api\"`")),
    );
}

/// Fabro's `backend_valid`, the missing-command clause: an agent on
/// `backend="acp"` names its agent through `acp.command` or `acp.config`,
/// on the node or on the graph; without one the step fails at run time.
pub(super) fn acp_requires_command(node: &NodeDecl, diags: &mut Diagnostics) {
    diags.push(
        Diagnostic::error(
            "attractor.acp_requires_command",
            node.attrs.span_of("backend", &node.span),
            format!(
                "node `{}` runs on `backend=\"acp\"` but names no ACP agent",
                node.id
            ),
        )
        .with_hint("set `acp.command` or `acp.config` on the node or on the graph"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd_absolute_matches_a_cd_into_root_and_not_a_relative_one() {
        assert!(CD_ABSOLUTE.is_match("cd /tmp && make"));
        assert!(CD_ABSOLUTE.is_match("(cd\t/srv; ls)"));
        assert!(CD_ABSOLUTE.is_match("make\ncd  /opt"));
        assert!(!CD_ABSOLUTE.is_match("cd src && make"));
        assert!(!CD_ABSOLUTE.is_match("abcd /x"));
        assert!(!CD_ABSOLUTE.is_match("echo cd/"));
    }
}
