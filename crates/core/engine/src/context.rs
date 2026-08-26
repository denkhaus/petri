//! Building the [`EvalEnv`] a firing's expressions see, and resolving HIR config
//! placeholders against it.
//!
//! There is exactly one way for an expression to reach upstream state: `nodes.*` and
//! `kv.*` on the [`RunContext`], which only `apply` writes. Nothing is threaded
//! through token payloads for that purpose, and no second bag of upstream bindings
//! exists.
//!
//! An expression sees three things:
//!
//! - the **token**: the payload that arrived on the first input edge,
//! - the **run context**: every completed node instance, and run-scoped `kv`,
//! - the **statics**: scope `env`, the node's identity, generation and attempt, the
//!   firing's own outcome where there is one, and `item` / `index` in a clone.

use std::collections::BTreeMap;

use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{
    Attempt, EvalEnv, EvalError, ExprId, ExprOrValue, Generation, NodeId, Outcome, RunContext,
    StaticCtx, Status, Token, Value, eval,
};
use serde_json::Map;
use smol_str::SmolStr;

use crate::state::{EngineState, RunError};

/// The static bindings for a firing, before it runs.
pub(crate) fn firing_statics(
    state: &EngineState,
    node: NodeId,
    inputs: &[Token],
    generation: Generation,
    attempt: Attempt,
) -> Result<StaticCtx, RunError> {
    let nd = state.graph.node(node).ok_or(RunError::UnknownNode(node))?;

    let mut ctx = StaticCtx::new();
    let empty_run = RunContext::new();

    // Run parameters first, so everything below shadows them.
    for (key, value) in &state.graph.params {
        ctx.set(key, value.clone());
    }

    // Scope env. An env expression sees nothing but itself, so env can never depend
    // on its own resolution order.
    let mut env = Map::new();
    if let Some(scope) = state.graph.scope(nd.scope) {
        for (key, value) in &scope.env {
            let resolved = match value {
                ExprOrValue::Value(v) => v.clone(),
                ExprOrValue::Expr(id) => {
                    // Scope env sees the run parameters and nothing else.
                    let mut params_only = StaticCtx::new();
                    for (key, value) in &state.graph.params {
                        params_only.set(key, value.clone());
                    }
                    let env = EvalEnv::new(&Value::Null, &empty_run, &params_only);
                    eval(&state.graph.exprs, *id, &env).map_err(|error| RunError::Eval {
                        node,
                        site: SmolStr::new(format!("scope env `{key}`")),
                        error,
                    })?
                }
            };
            env.insert(key.to_string(), resolved);
        }
    }
    ctx.set("env", Value::Object(env));

    // Every input payload, for a collector assembling results from a join. The
    // primary payload is the token, and comes from `EvalEnv`, not from here.
    ctx.set(
        "inputs",
        Value::Array(inputs.iter().map(|t| t.payload.clone()).collect()),
    );

    // The folded status of this node's upstream, read from the run context: failure
    // if any upstream failed, skipped if none ran, else success. Statuses are read by
    // node name through `nodes.*`; nothing rides on the token payload.
    ctx.set(
        "status",
        Value::String(folded_upstream_status(state, inputs).to_string()),
    );

    ctx.set("generation", Value::from(generation.raw()));
    ctx.set("attempt", Value::from(attempt.raw()));
    ctx.set("node", Value::String(nd.name.to_string()));
    // `run.failed` means "any failure so far" under every completion policy — not
    // the folded status, which under `Completion::TerminalNode` reads `Failed`
    // until the exit record exists and would poison these guards mid-run.
    ctx.set(
        "run",
        serde_json::json!({
            "failed": state.any_failure(),
            "cancelled": state.is_cancelled(),
        }),
    );
    // `run.cancelled` is root-only, so a `fail_fast` scope cancel is invisible to
    // it. This one is true whenever the firing's node lies in a cancelled
    // cancel-scope; a root cancel marks every scope, so it subsumes `run.cancelled`
    // for gating.
    ctx.set(
        "scope_cancelled",
        Value::Bool(state.is_node_cancelled(node)),
    );

    // `item` and `index` for a node that came out of an expansion.
    if let Some(bindings) = state.clone_bindings_for(node) {
        for (key, value) in bindings {
            ctx.set(key, value.clone());
        }
    }

    Ok(ctx)
}

/// The statics plus the firing's own result: what routing sees.
pub(crate) fn with_outcome(base: &StaticCtx, outcome: &Outcome) -> StaticCtx {
    let mut ctx = base.clone();
    ctx.set("status", Value::String(outcome.status.tag().to_string()));
    ctx.set("output", outcome.output.clone());
    ctx.set(
        "outcome",
        serde_json::json!({
            "status": outcome.status.tag(),
            "output": outcome.output.clone(),
            "success_like": outcome.status.is_success_like(),
        }),
    );
    ctx
}

/// The token an expression reads: the payload of the first input edge.
pub(crate) fn primary_token(inputs: &[Token]) -> Value {
    inputs
        .first()
        .map(|t| t.payload.clone())
        .unwrap_or(Value::Null)
}

/// Fold the statuses of the nodes whose edges fed this firing.
///
/// Read from [`RunContext`], by looking up the source node of each input edge. With
/// no upstream at all — an entry node, or a clone seeded by a splice — the fold is a
/// success.
fn folded_upstream_status(state: &EngineState, inputs: &[Token]) -> &'static str {
    let mut saw_any = false;
    let mut any_failed = false;
    let mut any_cancelled = false;
    let mut any_ran = false;
    for token in inputs {
        let Some(source) = state.graph.edge_source(token.edge) else {
            continue;
        };
        let Some(node) = state.graph.node(source) else {
            continue;
        };
        let Some(record) = state.run_context().node(&node.name) else {
            continue;
        };
        saw_any = true;
        any_failed |= record.status.is_failure();
        any_cancelled |= matches!(record.status, Status::Cancelled);
        any_ran |= !matches!(record.status, Status::Skipped);
    }
    // failure > cancelled > skipped > success: an upstream failure still reads as
    // "failure" even when a cancel also landed, and a cancelled upstream makes the
    // core `success()` guard false and `cancelled()` true.
    if any_failed {
        "failure"
    } else if any_cancelled {
        "cancelled"
    } else if !saw_any || any_ran {
        "success"
    } else {
        "skipped"
    }
}

/// Replace `{"$expr": <id>}` placeholders in a step config with their values.
///
/// This is the lazy half of HIR lowering: config is bound against the live
/// environment at firing time, not at load time.
pub(crate) fn resolve_config(
    config: &Value,
    exprs: &ir::ExprTable,
    env: &EvalEnv<'_>,
) -> Result<Value, EvalError> {
    match config {
        Value::Object(map) => {
            if let Some(id) = map.get(EXPR_PLACEHOLDER_KEY).and_then(placeholder_id) {
                return eval(exprs, id, env);
            }
            let mut out = Map::with_capacity(map.len());
            for (key, value) in map {
                out.insert(key.clone(), resolve_config(value, exprs, env)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .iter()
            .map(|i| resolve_config(i, exprs, env))
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
