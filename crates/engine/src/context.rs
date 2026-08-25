//! Building the expression context for a firing, and resolving HIR config
//! placeholders against it.
//!
//! Two contexts exist per firing. The **firing context** is what a precondition and
//! a step's config see: inputs, upstream statuses, scope env, prior outputs. The
//! **outcome context** adds the firing's own result and is what routing guards and
//! `map` expressions see.

use std::collections::BTreeMap;

use ir::validate::EXPR_PLACEHOLDER_KEY;
use ir::{
    Context, EvalError, ExprId, ExprOrValue, Generation, NodeId, Outcome, Token, Value, eval,
};
use serde_json::Map;
use smol_str::SmolStr;

use crate::state::{EngineState, RunError};

/// Bindings visible before the node runs.
pub(crate) fn firing_context(
    state: &EngineState,
    node: NodeId,
    inputs: &[Token],
    generation: Generation,
) -> Result<Context, RunError> {
    let nd = state
        .graph
        .node(node)
        .ok_or(RunError::UnknownNode(node))?
        .clone();

    let mut ctx = Context::new();

    // Scope env. Expressions in env see only the plain bindings, never each other,
    // so env can never depend on its own resolution order.
    let mut env = Map::new();
    if let Some(scope) = state.graph.scope(nd.scope) {
        for (key, value) in &scope.env {
            let resolved =
                match value {
                    ExprOrValue::Value(v) => v.clone(),
                    ExprOrValue::Expr(id) => eval(&state.graph.exprs, *id, &Context::new())
                        .map_err(|error| RunError::Eval {
                            node,
                            site: SmolStr::new(format!("scope env `{key}`")),
                            error,
                        })?,
                };
            env.insert(key.to_string(), resolved);
        }
    }
    ctx.set("env", Value::Object(env));

    ctx.set(
        "outputs",
        Value::Object(
            state
                .outputs()
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        ),
    );

    let payloads: Vec<Value> = inputs.iter().map(|t| t.payload.clone()).collect();
    ctx.set("input", payloads.first().cloned().unwrap_or(Value::Null));
    ctx.set("inputs", Value::Array(payloads));

    // Upstream statuses, so a precondition can ask `failure()` the way GHA's `if:`
    // does. With no upstream at all (an entry node) the fold is a success.
    let mut upstream = Vec::new();
    let mut any_failed = false;
    let mut any_ran = false;
    for token in inputs {
        if let Some(record) = state
            .history()
            .iter()
            .rev()
            .find(|r| r.firing == token.from)
        {
            let tag = record.outcome.status.tag();
            any_failed |= record.outcome.status.is_failure();
            any_ran |= !matches!(record.outcome.status, ir::Status::Skipped);
            upstream.push(serde_json::json!({
                "node": record.name.to_string(),
                "status": tag,
                "output": record.outcome.output.clone(),
            }));
        }
    }
    let folded = if any_failed {
        "failure"
    } else if upstream.is_empty() || any_ran {
        "success"
    } else {
        "skipped"
    };
    ctx.set("upstream", Value::Array(upstream));
    ctx.set("status", Value::String(folded.to_string()));

    ctx.set("generation", Value::from(generation.raw()));
    ctx.set("node", Value::String(nd.name.to_string()));
    ctx.set(
        "run",
        serde_json::json!({
            "failed": matches!(state.folded_status(), ir::RunStatus::Failed),
            "cancelled": matches!(state.folded_status(), ir::RunStatus::Cancelled),
        }),
    );

    // `item` and `index` for a node that came out of an expansion.
    if let Some(bindings) = state.clone_bindings_for(node) {
        for (key, value) in bindings {
            ctx.set(key, value.clone());
        }
    }

    Ok(ctx)
}

/// The firing context plus the firing's own result: what routing sees.
pub(crate) fn outcome_context(base: &Context, outcome: &Outcome) -> Context {
    let mut ctx = base.clone();
    ctx.set("status", Value::String(outcome.status.tag().to_string()));
    ctx.set("output", outcome.output.clone());
    ctx.set(
        "outcome",
        serde_json::json!({
            "status": outcome.status.tag(),
            "output": outcome.output.clone(),
        }),
    );
    ctx
}

/// Replace `{"$expr": <id>}` placeholders in a step config with their values.
///
/// This is the lazy half of HIR lowering: config is bound against live contexts at
/// firing time, not at load time.
pub(crate) fn resolve_config(
    config: &Value,
    exprs: &ir::ExprTable,
    ctx: &Context,
) -> Result<Value, EvalError> {
    match config {
        Value::Object(map) => {
            if let Some(id) = map.get(EXPR_PLACEHOLDER_KEY).and_then(placeholder_id) {
                return eval(exprs, id, ctx);
            }
            let mut out = Map::with_capacity(map.len());
            for (key, value) in map {
                out.insert(key.clone(), resolve_config(value, exprs, ctx)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .iter()
            .map(|i| resolve_config(i, exprs, ctx))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

fn placeholder_id(value: &Value) -> Option<ExprId> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .map(ExprId::new)
}

/// The `item` / `index` pair bound into an expansion clone.
pub(crate) fn clone_bindings(index: u32, item: &Value) -> BTreeMap<SmolStr, Value> {
    BTreeMap::from([
        (SmolStr::new("item"), item.clone()),
        (SmolStr::new("index"), Value::from(index)),
    ])
}
