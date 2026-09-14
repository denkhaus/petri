//! Failure promotion, as Fabro orders it.
//!
//! Fabro promotes a failed stage under `on_failure="succeed"` only when no
//! explicit route matches the failed outcome. An explicit route is a
//! conditional edge, a preferred label, or a suggested target. The check runs
//! against the prospective context: the run context with the stage's own
//! updates applied. Petri classifies a stage's outcome at the step boundary,
//! before routing, so the step needs the node's explicit routes to make the
//! same decision. This module builds that list; the step evaluates it
//! (`attractor_steps::outcome`).

use serde_json::{Value, json};

use crate::labels;
use crate::model::{NodeDecl, Workflow};

/// The config key the explicit routes ride under.
pub const ROUTES_KEY: &str = "routes";

/// The node's explicit routes: every condition text, every label key of an
/// unconditional labelled edge, and every unconditional edge's target.
pub(super) fn explicit_routes(node: &NodeDecl, workflow: &Workflow) -> Value {
    let mut conditions = Vec::new();
    let mut labels = Vec::new();
    let mut targets = Vec::new();
    for edge in workflow.outgoing(&node.id) {
        let condition = edge
            .attrs
            .text("condition")
            .filter(|text| !text.trim().is_empty());
        if let Some(condition) = condition {
            conditions.push(Value::String(condition));
            continue;
        }
        if let Some(label) = edge.attrs.text("label").filter(|l| !l.is_empty()) {
            labels.push(Value::String(labels::routing_key(&label)));
        }
        targets.push(Value::String(edge.to.clone()));
    }
    json!({
        "conditions": conditions,
        "labels": labels,
        "targets": targets,
    })
}
