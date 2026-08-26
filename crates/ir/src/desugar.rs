//! Desugaring: frontend helpers for the two `for_each` shapes of §6.
//!
//! Sequential iteration uses **no new IR**: it desugars onto back edges and
//! generations ([`sequential_for_each`]). Parallel iteration uses [`Expansion`],
//! which the engine splices into the live graph ([`parallel_for_each`]).
//!
//! Both live here rather than in the engine, because they are surface syntax.
//! Loops always flatten into the one graph; only the syntax is hierarchical.

use crate::builder::{Arm, GraphBuilder};
use crate::expr::{BinOp, ExprTable};
use crate::graph::{Budget, ExpandTarget, Expansion, Graph, JoinPolicy};
use crate::ids::{EdgeId, ExprId, NodeId};

/// The expressions a sequential `for_each` needs. The loop state is one object,
/// `{ items, idx, acc }`, threaded along the loop's edges.
#[derive(Clone, Copy, Debug)]
pub struct LoopExprs {
    /// `{ items: output, idx: 0, acc: [] }` — the state the entry node emits.
    pub init: ExprId,
    /// `input.items[input.idx]` — the element the current iteration works on.
    pub item: ExprId,
    /// `input.idx` — the current position.
    pub index: ExprId,
    /// `input.idx + 1 < len(input.items)` — the back edge's guard.
    pub more: ExprId,
    /// `{ items, idx: idx + 1, acc: acc ++ [output] }` — the back edge's payload.
    pub next: ExprId,
    /// `input.acc ++ [output]` — the exit edge's payload: every result, in order.
    pub result: ExprId,
}

/// Build the loop-state expressions once; reuse the ids across the loop's edges.
/// The items are the source node's whole `output`.
pub fn loop_exprs(t: &mut ExprTable) -> LoopExprs {
    let output = t.var("output");
    loop_exprs_over(t, output)
}

/// Build the loop-state expressions with `items` as the array to iterate, evaluated
/// in the source node's outcome context.
pub fn loop_exprs_over(t: &mut ExprTable, items_expr: ExprId) -> LoopExprs {
    let output = t.var("output");
    let zero = t.lit(0);
    let empty = t.array(vec![]);
    let init = t.object(vec![("items", items_expr), ("idx", zero), ("acc", empty)]);

    let items = t.path("input", &["items"]);
    let index = t.path("input", &["idx"]);
    let acc = t.path("input", &["acc"]);
    let item = t.index(items, index);

    let one = t.lit(1);
    let next_index = t.binary(BinOp::Add, index, one);
    let length = t.call("len", vec![items]);
    let more = t.binary(BinOp::Lt, next_index, length);

    let output_one = t.array(vec![output]);
    let next_acc = t.binary(BinOp::Concat, acc, output_one);
    let next = t.object(vec![
        ("items", items),
        ("idx", next_index),
        ("acc", next_acc),
    ]);

    LoopExprs {
        init,
        item,
        index,
        more,
        next,
        result: next_acc,
    }
}

/// What [`sequential_for_each`] wired up.
#[derive(Clone, Debug)]
pub struct SequentialForEach {
    pub exprs: LoopExprs,
    /// Edge from the source node into the loop head.
    pub enter: EdgeId,
    /// The back edge. Crossing it bumps the generation.
    pub back: EdgeId,
    /// Edge from the loop tail to the collector, carrying every result in order.
    pub exit: EdgeId,
}

/// Desugar `for_each ... parallel: false` into a cycle.
///
/// `source` must output the items array. The body runs between `head` and `tail`
/// (they may be the same node) and must carry the loop state on its payload; read
/// the current element through [`LoopExprs::item`].
///
/// `head` is set to [`JoinPolicy::Any`], because on the first iteration only the
/// entry edge carries a token and on later ones only the back edge does. Every node
/// in the loop is capped at `max_iterations` firings, which is what makes the run
/// terminate (invariant 4).
pub fn sequential_for_each(
    b: &mut GraphBuilder,
    source: NodeId,
    head: NodeId,
    tail: NodeId,
    collector: NodeId,
    max_iterations: u32,
) -> SequentialForEach {
    let output = b.exprs().var("output");
    sequential_for_each_over(b, source, head, tail, collector, max_iterations, output)
}

/// [`sequential_for_each`] with an explicit `items` expression, evaluated in the
/// source node's outcome context, instead of the source's whole `output`.
pub fn sequential_for_each_over(
    b: &mut GraphBuilder,
    source: NodeId,
    head: NodeId,
    tail: NodeId,
    collector: NodeId,
    max_iterations: u32,
    items: ExprId,
) -> SequentialForEach {
    let exprs = loop_exprs_over(b.exprs(), items);

    let enter = {
        let ids = b.select(source, vec![Arm::always(head).with_map(exprs.init)]);
        ids[0]
    };

    // One group, two arms: loop back while there is more, otherwise leave. Because
    // it is one group, exactly one of them fires.
    let ids = b.select(
        tail,
        vec![
            Arm::when(head, exprs.more).with_map(exprs.next).as_back(),
            Arm::always(collector).with_map(exprs.result),
        ],
    );

    b.set_join(head, JoinPolicy::Any);
    b.set_budget(head, Budget::looped(max_iterations));
    if tail != head {
        b.set_budget(tail, Budget::looped(max_iterations));
    }

    SequentialForEach {
        exprs,
        enter,
        back: ids[0],
        exit: ids[1],
    }
}

/// The expressions a parallel `for_each` collector needs.
#[derive(Clone, Copy, Debug)]
pub struct CollectorExprs {
    /// `{ index, value: output }` — the payload each clone sends to the collector.
    pub indexed: ExprId,
    /// `pluck(sort_by_key(inputs, "index"), "value")` — every clone result, back in
    /// `items` order.
    pub ordered: ExprId,
}

/// Build the collector expressions. Put [`CollectorExprs::indexed`] on the edge out
/// of the expanded node, and [`CollectorExprs::ordered`] on the edge out of the
/// collector.
pub fn collector_exprs(t: &mut ExprTable) -> CollectorExprs {
    let index = t.var("index");
    let output = t.var("output");
    let indexed = t.object(vec![("index", index), ("value", output)]);

    let inputs = t.var("inputs");
    let index_key = t.lit("index");
    let sorted = t.call("sort_by_key", vec![inputs, index_key]);
    let value_key = t.lit("value");
    let ordered = t.call("pluck", vec![sorted, value_key]);

    CollectorExprs { indexed, ordered }
}

/// Rewrite `JoinPolicy::Quorum { n: 1 }` to `Any` on every loop head.
///
/// `Quorum { n: 1 }` and `Any` behave identically today, but invariant 8 admits only
/// `Any` on a loop head: one canonical spelling is easier to grep and to review, and
/// the equivalence is a property of the current join semantics rather than a
/// guarantee worth making load-bearing. A frontend that naturally produces
/// `Quorum { n: 1 }` runs this pass instead of the invariant being relaxed.
///
/// Run it after lowering and before validating. Returns how many nodes it changed.
pub fn normalize_loop_heads(graph: &mut Graph) -> usize {
    let heads: Vec<NodeId> = graph
        .edges()
        .filter(|edge| edge.back)
        .map(|edge| edge.to)
        .collect();
    let mut changed = 0;
    for head in heads {
        if let Some(node) = graph.node_mut(head)
            && node.join == (JoinPolicy::Quorum { n: 1 })
        {
            node.join = JoinPolicy::Any;
            changed += 1;
        }
    }
    changed
}

/// Mark a node for parallel expansion: `for_each ... parallel: true`.
///
/// `items` must evaluate to an array. Each element gets a clone with `item` and
/// `index` bound, and every clone's outgoing edges are spliced into the collector,
/// so the collector's `All` join counts them all.
pub fn parallel_for_each(
    b: &mut GraphBuilder,
    node: NodeId,
    items: ExprId,
    target: ExpandTarget,
    max_parallel: Option<u32>,
    fail_fast: bool,
) {
    b.set_expansion(
        node,
        Expansion::ForEach {
            items,
            target,
            max_parallel,
            fail_fast,
        },
    );
}
