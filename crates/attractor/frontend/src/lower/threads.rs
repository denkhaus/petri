//! Fidelity, threads, project memory and model controls on agent and prompt
//! nodes: what the lowering carries so the step can resolve them at run time.
//!
//! Resolution needs the incoming edge, which is only known when a token
//! arrives. So every edge into an agent or prompt node maps its token payload
//! to `{ from, fidelity, thread_id }`, and the node's config reads that
//! payload back as `incoming`. The node's own attributes, the graph's
//! defaults and the node's classes ride in the config as literals. The rules
//! that combine them are `crate::fidelity`.

use std::collections::HashMap;

use frontend::{Diagnostic, Diagnostics, Span};
use ir::{ExprId, ExprTable, Value};
use serde_json::{Map, json};

use super::{Kind, NodeRef, attrs, placeholder};
use crate::fidelity::Fidelity;
use crate::model::{Attrs, EdgeDecl, NodeDecl, Workflow};

/// The validation rule Fabro names, as a warning code.
const THREAD_RULE: &str = "attractor.thread_id_requires_fidelity_full";
/// Fabro's lint for a `thread_id` on a parallel branch, where it is inert,
/// and for a `fidelity="full"` there, which the branch degrades.
const BRANCH_INERT: &str = "attractor.parallel_branch_inert_attribute";

/// The `fidelity="full"` clause of Fabro's `parallel_branch_inert_attribute`:
/// a branch runs at most at `summary:high`, because concurrent branches
/// cannot share a conversation.
fn branch_full_fidelity(what: &str, span: Span, diags: &mut Diagnostics) {
    diags.push(
        Diagnostic::warning(
            BRANCH_INERT,
            span,
            format!(
                "`fidelity=\"full\"` on {what} is degraded: a parallel branch runs at most at \
                 `summary:high`, because concurrent branches cannot share a conversation"
            ),
        )
        .with_hint(
            "use `fidelity=\"summary:high\"` or a lower mode on the branch; to reuse a full \
             session before the fan-out, set `fidelity=\"full\"` on the parallel node or its \
             incoming edge",
        ),
    );
}

/// Parse a `fidelity` attribute on `attrs`, diagnosing a bad mode as an
/// error. `None` when absent or bad.
pub(super) fn fidelity_attr(
    attrs: &Attrs,
    key: &str,
    span: &Span,
    what: &str,
    diags: &mut Diagnostics,
) -> Option<Fidelity> {
    let value = attrs.text(key)?;
    let parsed = value.parse::<Fidelity>().ok();
    if parsed.is_none() {
        diags.error(
            "attractor.bad_fidelity",
            attrs.span_of(key, span),
            format!(
                "`{value}` on {what} is not a fidelity mode ({})",
                attrs::FIDELITIES.join(", ")
            ),
        );
    }
    parsed
}

/// The literal part of an agent or prompt node's thread configuration.
pub(super) struct ThreadAttrs {
    pub fidelity:         Option<Fidelity>,
    pub default_fidelity: Option<Fidelity>,
    pub thread_id:        Option<String>,
    pub default_thread:   Option<String>,
    pub classes:          Vec<String>,
    pub project_memory:   bool,
    pub speed:            Option<String>,
    pub max_tokens:       Option<i64>,
}

impl ThreadAttrs {
    /// Read the node and graph attributes, with Fabro's
    /// `thread_id_requires_fidelity_full` warnings. `branch_first` says the
    /// node is entered directly from a parallel fork, where a `thread_id` is
    /// inert.
    pub(super) fn read(
        node: &NodeDecl,
        workflow: &Workflow,
        branch_first: bool,
        default_speed: Option<&str>,
        diags: &mut Diagnostics,
    ) -> Self {
        let fidelity = fidelity_attr(
            &node.attrs,
            "fidelity",
            &node.span,
            &format!("node `{}`", node.id),
            diags,
        );
        let graph_fidelity = fidelity_attr(
            &workflow.attrs,
            "default_fidelity",
            &workflow.span,
            "the graph",
            diags,
        );
        let thread_id = node.attrs.text("thread_id").filter(|t| !t.is_empty());
        let default_thread = workflow
            .attrs
            .text("default_thread")
            .filter(|t| !t.is_empty());
        if thread_id.is_some() {
            if branch_first {
                diags.warning(
                    BRANCH_INERT,
                    node.attrs.span_of("thread_id", &node.span),
                    format!(
                        "`thread_id` on `{}` is inert: the node is entered directly from a \
                         parallel fork, and concurrent branches cannot share a conversation",
                        node.id
                    ),
                );
            } else if fidelity != Some(Fidelity::Full) && graph_fidelity != Some(Fidelity::Full) {
                diags.warning(
                    THREAD_RULE,
                    node.attrs.span_of("thread_id", &node.span),
                    format!(
                        "Node '{}' has thread_id but fidelity is not 'full'. Add \
                         fidelity=\"full\" to enable session reuse, or remove thread_id",
                        node.id
                    ),
                );
            }
        }
        if branch_first && fidelity == Some(Fidelity::Full) {
            branch_full_fidelity(
                &format!("`{}`", node.id),
                node.attrs.span_of("fidelity", &node.span),
                diags,
            );
        }
        let project_memory = node.attrs.bool("project_memory", diags).unwrap_or(true);
        let speed = node
            .attrs
            .text("speed")
            .filter(|s| !s.is_empty())
            .or_else(|| default_speed.map(str::to_owned));
        if let Some(speed) = &speed
            && !matches!(speed.as_str(), "standard" | "fast")
        {
            diags.error(
                "attractor.bad_speed",
                node.attrs.span_of("speed", &node.span),
                format!(
                    "Invalid speed \"{speed}\" for node \"{}\"; expected one of: standard, fast",
                    node.id
                ),
            );
        }
        let max_tokens = node
            .attrs
            .int("max_tokens", diags)
            .filter(|tokens| *tokens > 0);
        Self {
            fidelity,
            default_fidelity: graph_fidelity,
            thread_id,
            default_thread,
            classes: node.classes.clone(),
            project_memory,
            speed,
            max_tokens,
        }
    }

    /// Write the literals into the step config, plus the `incoming`
    /// placeholder that reads the arriving token.
    pub(super) fn write(self, exprs: &mut ExprTable, config: &mut Map<String, Value>) {
        if let Some(fidelity) = self.fidelity {
            config.insert("fidelity".into(), json!(fidelity.as_str()));
        }
        if let Some(fidelity) = self.default_fidelity {
            config.insert("default_fidelity".into(), json!(fidelity.as_str()));
        }
        if let Some(thread) = self.thread_id {
            config.insert("thread_id".into(), json!(thread));
        }
        if let Some(thread) = self.default_thread {
            config.insert("default_thread".into(), json!(thread));
        }
        config.insert("classes".into(), json!(self.classes));
        if !self.project_memory {
            config.insert("project_memory".into(), json!(false));
        }
        if let Some(speed) = self.speed {
            config.insert("speed".into(), json!(speed));
        }
        if let Some(tokens) = self.max_tokens {
            config.insert("max_tokens".into(), json!(tokens));
        }
        let incoming = exprs.var("input");
        config.insert("incoming".into(), placeholder(incoming));
    }
}

/// The graph-level `default_thread` warning: Fabro warns when it is set
/// without `default_fidelity="full"`.
pub(super) fn check_graph(workflow: &Workflow, diags: &mut Diagnostics) {
    if workflow
        .attrs
        .text("default_thread")
        .is_some_and(|t| !t.is_empty())
        && workflow.attrs.text("default_fidelity").as_deref() != Some("full")
    {
        diags.warning(
            THREAD_RULE,
            workflow.attrs.span_of("default_thread", &workflow.span),
            "Graph has default_thread but default_fidelity is not 'full'. Add \
             default_fidelity=\"full\" to enable session reuse, or remove default_thread",
        );
    }
}

/// The checks on an edge out of a parallel node, which starts a branch and
/// carries no payload: a bad `fidelity` is an error, a `thread_id` is inert
/// and `fidelity="full"` is degraded (Fabro's
/// `parallel_branch_inert_attribute`).
pub(super) fn check_fork_edge(edge: &EdgeDecl, diags: &mut Diagnostics) {
    let what = format!("edge `{} -> {}`", edge.from, edge.to);
    let fidelity = fidelity_attr(&edge.attrs, "fidelity", &edge.span, &what, diags);
    if edge.attrs.text("thread_id").is_some_and(|t| !t.is_empty()) {
        diags.warning(
            BRANCH_INERT,
            edge.attrs.span_of("thread_id", &edge.span),
            format!("`thread_id` on {what} is inert: a fork edge starts a parallel branch"),
        );
    }
    if fidelity == Some(Fidelity::Full) {
        branch_full_fidelity(&what, edge.attrs.span_of("fidelity", &edge.span), diags);
    }
}

/// The payload an edge into an agent or prompt node carries: which node the
/// token left, and the edge's own `fidelity` and `thread_id`. `None` when the
/// target is not an agent or prompt node, or the edge leaves a parallel fork
/// (a branch's first node inherits the fork's preamble instead).
pub(super) fn edge_payload(
    exprs: &mut ExprTable,
    edge: &EdgeDecl,
    nodes: &HashMap<String, NodeRef>,
    graph_fidelity_full: bool,
    diags: &mut Diagnostics,
) -> Option<ExprId> {
    let target = nodes.get(&edge.to)?;
    if !target.kind.is_llm() {
        return None;
    }
    if nodes.get(&edge.from).map(|n| n.kind) == Some(Kind::Parallel) {
        // A fork edge is the parallel lowering's; `check_fork_edge` diagnoses it.
        return None;
    }
    let what = format!("edge `{} -> {}`", edge.from, edge.to);
    let fidelity = fidelity_attr(&edge.attrs, "fidelity", &edge.span, &what, diags);
    let thread = edge.attrs.text("thread_id").filter(|t| !t.is_empty());
    if thread.is_some() && fidelity != Some(Fidelity::Full) && !graph_fidelity_full {
        diags.warning(
            THREAD_RULE,
            edge.attrs.span_of("thread_id", &edge.span),
            format!(
                "Edge {} -> {} has thread_id but fidelity is not 'full'. Add fidelity=\"full\" \
                 to enable session reuse, or remove thread_id",
                edge.from, edge.to
            ),
        );
    }
    let from = exprs.lit(edge.from.as_str());
    let fidelity = exprs.lit(fidelity.map_or(Value::Null, |f| json!(f.as_str())));
    let thread = exprs.lit(thread.map_or(Value::Null, Value::String));
    Some(exprs.object(vec![
        ("from", from),
        ("fidelity", fidelity),
        ("thread_id", thread),
    ]))
}

/// Whether `node` is entered directly from a parallel fork.
pub(super) fn is_branch_first(
    node: &NodeDecl,
    workflow: &Workflow,
    nodes: &HashMap<String, NodeRef>,
) -> bool {
    workflow
        .incoming(&node.id)
        .iter()
        .any(|edge| nodes.get(&edge.from).map(|n| n.kind) == Some(Kind::Parallel))
}

/// A stage description for the preamble: every node's kind, and for a
/// command its script, keyed by Fabro node id, in declaration order.
pub(super) fn stages(workflow: &Workflow, nodes: &HashMap<String, NodeRef>) -> Value {
    Value::Array(
        workflow
            .nodes
            .iter()
            .map(|node| {
                let mut stage = Map::new();
                stage.insert("id".into(), json!(node.id));
                if let Some(declared) = nodes.get(&node.id) {
                    stage.insert("kind".into(), json!(declared.kind.name()));
                }
                if let Some(script) = node.attrs.text("script") {
                    stage.insert("script".into(), json!(script));
                }
                if let Some(model) = node.attrs.text("model") {
                    stage.insert("model".into(), json!(model));
                }
                Value::Object(stage)
            })
            .collect(),
    )
}
