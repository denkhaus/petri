//! Seed edges: the synthetic incoming edges for entry nodes and expansion clone
//! entries. They exist so the firing rule needs no special case for a node with
//! no declared incoming edge.

mod support;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use ir::{
    Arm, EdgeId, ExpandTarget, GraphBuilder, JoinPolicy, Outcome, RunStatus, Value,
    collector_exprs, parallel_for_each, validate,
};
use serde_json::json;
use support::{Harness, NOOP};

/// An entry node with `JoinPolicy::All` fires from exactly its seed token.
/// `All` over one seed edge is satisfied by one token, so the policy needs no
/// exception.
#[test]
fn an_all_join_entry_node_fires_from_its_seed_token_alone() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let entry = b.add_step("entry", scope, NOOP);
    let next = b.add_step("next", scope, NOOP);
    b.set_join(entry, JoinPolicy::All);
    b.link(entry, next);
    let graph = b.build();
    validate(&graph).expect("valid");
    assert_eq!(graph.in_degree(entry), 0, "no declared incoming edge");

    let inputs = Rc::new(RefCell::new(Vec::new()));
    let sink = inputs.clone();
    let mut h = Harness::new(graph).respond_with(move |info| {
        if info.base == "entry" {
            *sink.borrow_mut() = info.inputs.clone();
        }
        Outcome::success(Value::Null)
    });
    assert_eq!(h.run(), RunStatus::Success);

    let inputs = inputs.borrow();
    assert_eq!(inputs.len(), 1, "exactly one token, the seed");
    let seeds: Vec<EdgeId> = h.state.seed_edges().map(|(edge, _)| edge).collect();
    assert!(seeds.contains(&inputs[0].edge));
    assert_eq!(inputs[0].generation, ir::Generation::ZERO);
    assert_eq!(h.start_count("entry"), 1);
}

/// Every join policy works on an entry node, because the seed edge is counted
/// like any other incoming edge.
#[test]
fn every_join_policy_works_on_an_entry_node() {
    for join in [JoinPolicy::All, JoinPolicy::Any, JoinPolicy::Quorum {
        n: 1,
    }] {
        let mut b = GraphBuilder::new();
        let scope = ir::ScopeId::new(0);
        let entry = b.add_step("entry", scope, NOOP);
        b.set_join(entry, join);
        let graph = b.build();
        validate(&graph).expect("valid");

        let mut h = Harness::new(graph);
        assert_eq!(h.run(), RunStatus::Success, "{join:?}");
        assert_eq!(h.start_count("entry"), 1, "{join:?}");
    }
}

/// Seed edges are allocated above every declared id, so they collide with
/// nothing, and they are never written into a routing group.
#[test]
fn seed_edges_stay_out_of_the_graph() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let work = b.add_step("work", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, work);
    b.select(work, vec![Arm::always(collect).with_map(collector.indexed)]);
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(&mut b, work, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let declared: BTreeSet<EdgeId> = graph.edges().map(|e| e.id).collect();
    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b", "c"])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    // One for the entry node, one per clone.
    let seeds: BTreeSet<EdgeId> = h.state.seed_edges().map(|(edge, _)| edge).collect();
    assert_eq!(seeds.len(), 4);

    // Invariant 5 counts declared edges; seed edges live outside that space.
    assert!(
        seeds.is_disjoint(&declared),
        "a seed edge reused a declared edge id"
    );
    assert!(
        !seeds.contains(&EdgeId::SEED),
        "the reserved sentinel is never allocated"
    );

    // The live graph grew clone edges, and none of them is a seed edge.
    let live: BTreeSet<EdgeId> = h.state.graph.edges().map(|e| e.id).collect();
    assert!(
        seeds.is_disjoint(&live),
        "a seed edge appeared in a routing group"
    );
    assert!(declared.is_subset(&live), "declared edges are still there");
}

/// A clone entry with `JoinPolicy::All` fires from its own seed edge, which is
/// what lets a spliced subgraph start without a declared incoming edge.
#[test]
fn clone_entries_are_seeded_the_same_way() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let setup = b.add_step("setup", scope, NOOP);
    let run = b.add_step("run", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, setup);
    b.link(setup, run);
    b.select(run, vec![Arm::always(collect).with_map(collector.indexed)]);
    b.set_join(setup, JoinPolicy::All);
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(
        &mut b,
        setup,
        items,
        ExpandTarget::Subgraph {
            entry: setup,
            exit:  run,
        },
        None,
        false,
    );
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!([1, 2])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("setup"), 2);

    // Each clone entry has its own seed edge; the inner `setup -> run` edge is a
    // real cloned edge, not a seed.
    let seeded_nodes: Vec<String> = h
        .state
        .seed_edges()
        .filter_map(|(_, node)| h.state.graph.node(node))
        .map(|n| n.name.to_string())
        .collect();
    assert!(seeded_nodes.contains(&"setup#0".to_string()));
    assert!(seeded_nodes.contains(&"setup#1".to_string()));
    assert!(!seeded_nodes.iter().any(|n| n.starts_with("run")));
}
