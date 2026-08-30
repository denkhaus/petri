//! §3/§4: tokens are matched per (node, generation) by the node's join policy.

mod support;

use ir::{Arm, BinOp, Budget, GraphBuilder, JoinPolicy, Outcome, RunStatus, Value, validate};
use support::{Harness, NOOP};

/// Three branches, one `All` join: the node fires once, when every incoming
/// edge has a token.
#[test]
fn all_waits_for_every_incoming_edge() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let branches: Vec<_> = (0..3)
        .map(|i| b.add_step(&format!("branch{i}"), scope, NOOP))
        .collect();
    let join = b.add_step("join", scope, NOOP);
    b.fan_out(start, &branches);
    for branch in &branches {
        b.link(*branch, join);
    }
    b.set_join(join, JoinPolicy::All);
    let graph = b.build();
    validate(&graph).expect("valid");
    assert_eq!(graph.in_degree(join), 3);

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("join"), 1);
    assert_eq!(h.started.last().map(String::as_str), Some("join"));
}

/// `Any` fires on the first token; later same-generation tokens are dropped.
#[test]
fn any_fires_once_on_the_first_token() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let fast = b.add_step("fast", scope, NOOP);
    let slow = b.add_step("slow", scope, NOOP);
    let race = b.add_step("race", scope, NOOP);
    b.fan_out(start, &[fast, slow]);
    b.link(fast, race);
    b.link(slow, race);
    b.set_join(race, JoinPolicy::Any);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("race"), 1, "the second token is dropped");
}

/// `Quorum { n }` fires as soon as `n` distinct incoming edges have a token.
/// Driven one event at a time, so the exact firing point is visible.
#[test]
fn quorum_fires_at_n_distinct_edges() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let branches: Vec<_> = (0..3)
        .map(|i| b.add_step(&format!("branch{i}"), scope, NOOP))
        .collect();
    let quorum = b.add_step("quorum", scope, NOOP);
    b.fan_out(start, &branches);
    for branch in &branches {
        b.link(*branch, quorum);
    }
    b.set_join(quorum, JoinPolicy::Quorum { n: 2 });
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(engine::Event::RunStarted);
    let starts = h.take_starts();
    assert_eq!(starts.len(), 1);
    h.finish(starts[0].0, Outcome::success(Value::Null));

    let branch_starts = h.take_starts();
    assert_eq!(branch_starts.len(), 3, "all three branches start");

    h.finish(branch_starts[0].0, Outcome::success(Value::Null));
    assert!(h.take_starts().is_empty(), "one token is not a quorum");

    h.finish(branch_starts[1].0, Outcome::success(Value::Null));
    let quorum_starts = h.take_starts();
    assert_eq!(
        quorum_starts.len(),
        1,
        "the second token reaches the quorum"
    );

    // The third branch's token arrives after the firing and is dropped.
    h.finish(branch_starts[2].0, Outcome::success(Value::Null));
    assert!(h.take_starts().is_empty());
    h.finish(quorum_starts[0].0, Outcome::success(Value::Null));

    assert_eq!(h.status, Some(RunStatus::Success));
    assert_eq!(h.start_count("quorum"), 1);
    assert_eq!(h.state.pending_count(), 0);
}

/// A join sees only tokens of its own generation. A back edge bumps the
/// generation, so an old token never pairs with a new one.
#[test]
fn generations_partition_the_join() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let seed = b.add_step("seed", scope, NOOP);
    let head = b.add_step("head", scope, NOOP);
    let tail = b.add_step("tail", scope, NOOP);
    b.set_join(head, JoinPolicy::Any);
    b.set_budget(head, Budget::looped(4));
    b.set_budget(tail, Budget::looped(4));

    // seed emits 0; head passes it through; tail loops back while the counter is
    // under 2, so `head` fires in generations 0, 1 and 2.
    let (start_at_zero, bump, more) = {
        let e = b.exprs();
        let zero = e.lit(0);
        let output = e.var("output");
        let one = e.lit(1);
        let bump = e.binary(BinOp::Add, output, one);
        let two = e.lit(2);
        let more = e.binary(BinOp::Lt, output, two);
        (zero, bump, more)
    };
    let seed_edge = b.next_edge_id();
    b.node_mut(seed).routing =
        ir::Routing::next(ir::Edge::always(seed_edge, head).with_map(start_at_zero));
    b.link(head, tail);
    b.select(tail, vec![Arm::when(head, more).with_map(bump).as_back()]);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| Outcome::success(info.input()));
    assert_eq!(h.run(), RunStatus::Success);

    let generations: Vec<u32> = h
        .state
        .history()
        .iter()
        .filter(|r| r.name == "head")
        .map(|r| r.generation.raw())
        .collect();
    assert_eq!(generations, vec![0, 1, 2]);
}

/// An entry node has no incoming edges: it is seeded, and its join never
/// blocks.
#[test]
fn entry_nodes_are_seeded() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    let sink = b.add_step("sink", scope, NOOP);
    b.link(a, sink);
    b.link(c, sink);
    b.set_join(sink, JoinPolicy::All);
    b.mark_entry(a);
    b.mark_entry(c);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.max_concurrent, 2, "both entries start together");
    assert_eq!(h.start_count("sink"), 1);
    assert_eq!(h.output("sink"), Value::Null);
}
