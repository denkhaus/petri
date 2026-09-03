//! Lowering helpers: text in, graph or diagnostics out.

#![allow(
    dead_code,
    reason = "every test binary compiles this module whole, and no one of them uses every helper"
)]

use std::collections::BTreeMap;

use frontend::{CompileInputs, Diagnostic, FileSource, MapFiles, NoFiles};
use frontend_fabro::load;
use ir::{EvalEnv, ExprId, Graph, NodeId, RunContext, StaticCtx, Value, eval};
use serde_json::json;

pub(crate) fn files(pairs: &[(&str, &str)]) -> MapFiles {
    MapFiles(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
}

/// Wrap a graph body in the boilerplate every workflow has.
pub(crate) fn dot(body: &str) -> String {
    format!("digraph T {{\n  start [shape=Mdiamond]\n  exit [shape=Msquare]\n{body}\n}}")
}

pub(crate) fn lower_ok(text: &str) -> Graph {
    lower_ok_with(text, &NoFiles, &CompileInputs::new())
}

#[expect(
    clippy::print_stderr,
    reason = "the helper echoes the lowering's diagnostics so a graph that failed to lower \
              explains itself in the test output"
)]
pub(crate) fn lower_ok_with(text: &str, files: &dyn FileSource, inputs: &CompileInputs) -> Graph {
    let lowered = load("w.fabro", text, files, inputs);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("expected a graph")
}

pub(crate) fn diagnostics(text: &str) -> Vec<Diagnostic> {
    load("w.fabro", text, &NoFiles, &CompileInputs::new())
        .diagnostics
        .into_vec()
}

pub(crate) fn codes(text: &str) -> Vec<String> {
    let mut out: Vec<String> = diagnostics(text)
        .into_iter()
        .map(|d| d.code.to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

pub(crate) fn node<'a>(graph: &'a Graph, name: &str) -> &'a ir::Node {
    graph
        .nodes
        .iter()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("node `{name}`"))
}

pub(crate) fn node_id(graph: &Graph, name: &str) -> NodeId {
    node(graph, name).id
}

pub(crate) fn target_name(graph: &Graph, id: NodeId) -> String {
    graph.node(id).expect("target").name.to_string()
}

/// The tiers of the node's one routing group, as `(pick, [(target, guard)])`.
pub(crate) fn tiers(graph: &Graph, name: &str) -> Vec<(ir::PickPolicy, Vec<(String, ir::Guard)>)> {
    let node = node(graph, name);
    assert_eq!(node.routing.groups.len(), 1, "one group on `{name}`");
    let group = &node.routing.groups[0];
    let ir::SelectionPolicy::Tiered(tiers) = &group.policy else {
        panic!("`{name}` is not tiered");
    };
    tiers
        .iter()
        .map(|tier| {
            (
                tier.pick,
                tier.candidates
                    .iter()
                    .map(|c| {
                        let arm = group.arms.iter().find(|a| a.id == c.edge).expect("arm");
                        (target_name(graph, arm.to), c.when)
                    })
                    .collect(),
            )
        })
        .collect()
}

/// The statics a routing guard sees for one outcome.
pub(crate) fn statics(status: &str, output: &Value) -> StaticCtx {
    StaticCtx::new()
        .bind("status", json!(status))
        .bind("output", output.clone())
        .bind(
            "outcome",
            json!({ "status": status, "output": output.clone() }),
        )
}

pub(crate) fn eval_guard(
    graph: &Graph,
    guard: ir::Guard,
    statics: &StaticCtx,
    kv: &[(&str, Value)],
) -> bool {
    let ir::Guard::Expr(id) = guard else {
        return true;
    };
    eval_expr(graph, id, statics, kv)
        .as_bool()
        .expect("a guard is a boolean")
}

pub(crate) fn eval_expr(
    graph: &Graph,
    id: ExprId,
    statics: &StaticCtx,
    kv: &[(&str, Value)],
) -> Value {
    let mut run = RunContext::new();
    let updates: BTreeMap<smol_str::SmolStr, Value> = kv
        .iter()
        .map(|(k, v)| (smol_str::SmolStr::new(*k), v.clone()))
        .collect();
    run.merge(&updates);
    let token = Value::Null;
    eval(&graph.exprs, id, &EvalEnv::new(&token, &run, statics)).unwrap_or_else(|e| panic!("{e}"))
}
