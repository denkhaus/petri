//! §7: the invariants checked at load. Every check runs, so one call reports every
//! problem rather than stopping at the first.

use std::time::Duration;

use ir::validate::{EXPR_PLACEHOLDER_KEY, ValidationError, ValidationWarning};
use ir::{
    Arm, Budget, Edge, ExpandTarget, Expansion, ExprId, Graph, GraphBuilder, Guard, JoinPolicy,
    Node, NodeId, Routing, Scope, ScopeId, SelectGroup, StepKindId, StepRef, Value, validate,
    validate_plan,
};
use serde_json::json;

const NOOP: StepKindId = StepKindId::new(0);

fn errors(graph: &Graph) -> Vec<ValidationError> {
    validate(graph).expect_err("expected validation to fail")
}

/// A valid two-node graph, as a baseline.
#[test]
fn a_well_formed_graph_passes() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    validate(&b.build()).expect("valid");
}

/// Invariant 1: every cycle contains at least one back edge.
#[test]
fn a_cycle_without_a_back_edge_is_rejected() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    b.link(c, a);
    b.set_budget(a, Budget::looped(5));
    b.set_budget(c, Budget::looped(5));
    b.mark_entry(a);
    let graph = b.build();

    assert!(matches!(
        errors(&graph).as_slice(),
        [.., ValidationError::CycleWithoutBackEdge(_)]
            | [ValidationError::CycleWithoutBackEdge(_), ..]
    ));

    // Marking the return edge as a back edge fixes it.
    let mut b = GraphBuilder::new();
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.set_join(a, JoinPolicy::Any);
    b.link(a, c);
    b.select(c, vec![Arm::always(a).as_back()]);
    b.set_budget(a, Budget::looped(5));
    b.set_budget(c, Budget::looped(5));
    b.mark_entry(a);
    validate(&b.build()).expect("a back edge makes the cycle legal");
}

/// Invariant 2: `Guard::Always` may only be a group's final arm.
#[test]
fn always_must_be_the_last_arm() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let first = b.add_step("first", scope, NOOP);
    let second = b.add_step("second", scope, NOOP);
    b.select(a, vec![Arm::always(first), Arm::always(second)]);
    let graph = b.build();

    assert!(
        errors(&graph)
            .iter()
            .any(|e| matches!(e, ValidationError::AlwaysNotLast { arm: 0, .. }))
    );
}

/// Invariant 3: groups are non-empty, but a node may have no groups at all.
#[test]
fn groups_must_have_arms_but_a_node_need_not_have_groups() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    b.node_mut(a).routing = Routing::groups(vec![SelectGroup::new(vec![])]);
    let graph = b.build();
    assert!(
        errors(&graph)
            .iter()
            .any(|e| matches!(e, ValidationError::EmptyGroup { group: 0, .. }))
    );

    let mut b = GraphBuilder::new();
    b.add_step("terminal", scope, NOOP);
    validate(&b.build()).expect("a terminal node is fine");
}

/// Invariant 4: `max_firings >= 1`, and anything downstream of a back edge needs a
/// finite cap.
#[test]
fn budgets_must_be_at_least_one_and_finite_inside_loops() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    b.set_budget(a, Budget::new(0, Duration::from_secs(1)));
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::ZeroBudget(_)))
    );

    let mut b = GraphBuilder::new();
    let head = b.add_step("head", scope, NOOP);
    let tail = b.add_step("tail", scope, NOOP);
    b.set_join(head, JoinPolicy::Any);
    b.link(head, tail);
    b.select(tail, vec![Arm::always(head).as_back()]);
    b.set_budget(
        head,
        Budget::new(Budget::UNBOUNDED_FIRINGS, Duration::from_secs(1)),
    );
    b.mark_entry(head);
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::UnboundedLoopBudget(_)))
    );
}

/// Invariant 5: edge ids are unique, and the seed id is reserved.
#[test]
fn edge_ids_must_be_unique() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    b.fan_out(a, &[left, right]);
    let mut graph = b.build();
    // Force a collision.
    let clashing = graph.nodes[a.index()].routing.groups[0].arms[0].id;
    graph.nodes[a.index()].routing.groups[1].arms[0].id = clashing;
    assert!(
        errors(&graph)
            .iter()
            .any(|e| matches!(e, ValidationError::DuplicateEdgeId(_)))
    );

    let mut b = GraphBuilder::new();
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.node_mut(a).routing = Routing::next(Edge::always(ir::EdgeId::SEED, c));
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::ReservedEdgeId(_)))
    );
}

/// Invariant 6: expression references resolve.
#[test]
fn expression_references_must_resolve() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.select(a, vec![Arm::when(c, ExprId::new(99))]);
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::UnknownExpr { .. }))
    );
}

/// Invariant 6: HIR-only fields must be gone from an executable plan.
#[test]
fn a_plan_may_not_carry_hir_fields() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let items = b.exprs().lit(json!([1, 2]));
    b.set_expansion(
        a,
        Expansion::ForEach {
            items,
            target: ExpandTarget::Node,
            max_parallel: None,
            fail_fast: false,
        },
    );
    let graph = b.build();

    validate(&graph).expect("valid as HIR");
    let plan_errors = validate_plan(&graph).expect_err("not a plan");
    assert!(
        plan_errors
            .iter()
            .any(|e| matches!(e, ValidationError::HirFieldInPlan(_)))
    );

    // An unresolved config placeholder is the other half of the same rule.
    let mut b = GraphBuilder::new();
    let a = b.add_step("a", scope, NOOP);
    let value = b.exprs().lit(1);
    b.node_mut(a).step = StepRef::new(NOOP, json!({ "x": { EXPR_PLACEHOLDER_KEY: value.raw() } }));
    let graph = b.build();
    assert!(
        validate_plan(&graph)
            .expect_err("not a plan")
            .iter()
            .any(|e| matches!(e, ValidationError::HirConfigInPlan { .. }))
    );
}

/// Invariant 7: an edge into the middle of an expansion region is a boundary
/// crossing.
#[test]
fn expansion_regions_may_only_be_entered_at_the_entry() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let outside = b.add_step("outside", scope, NOOP);
    let entry = b.add_step("entry", scope, NOOP);
    let middle = b.add_step("middle", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(entry, middle);
    b.link(middle, exit);
    b.link(exit, after);
    // `outside` jumps straight into the middle of the region.
    b.link(outside, middle);
    let items = b.exprs().lit(json!([1]));
    b.set_expansion(
        entry,
        Expansion::ForEach {
            items,
            target: ExpandTarget::Subgraph { entry, exit },
            max_parallel: None,
            fail_fast: false,
        },
    );
    b.mark_entry(outside);
    b.mark_entry(entry);

    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::BoundaryCrossing { .. }))
    );
}

/// Invariant 7: the exit must postdominate the entry, so a region member that can
/// finish without reaching the exit is rejected.
#[test]
fn the_expansion_exit_must_postdominate_the_entry() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let entry = b.add_step("entry", scope, NOOP);
    let stops = b.add_step("stops", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    b.fan_out(entry, &[stops, exit]);
    let items = b.exprs().lit(json!([1]));
    b.set_expansion(
        entry,
        Expansion::ForEach {
            items,
            target: ExpandTarget::Subgraph { entry, exit },
            max_parallel: None,
            fail_fast: false,
        },
    );
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::ExitNotPostdominator { .. }))
    );
}

/// A well-formed expansion region passes.
#[test]
fn a_well_formed_expansion_region_passes() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let entry = b.add_step("entry", scope, NOOP);
    let middle = b.add_step("middle", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(entry, middle);
    b.link(middle, exit);
    b.link(exit, after);
    let items = b.exprs().lit(json!([1, 2]));
    b.set_expansion(
        entry,
        Expansion::ForEach {
            items,
            target: ExpandTarget::Subgraph { entry, exit },
            max_parallel: Some(2),
            fail_fast: true,
        },
    );
    validate(&b.build()).expect("valid");
}

/// Structure: node ids equal their index, targets exist, entries are seeded.
#[test]
fn structural_problems_are_reported() {
    let graph = Graph {
        nodes: vec![Node::new(
            NodeId::new(7),
            "misnumbered",
            ScopeId::new(0),
            StepRef::new(NOOP, Value::Null),
        )],
        scopes: vec![Scope::new(ScopeId::new(0))],
        exprs: Default::default(),
        entry: vec![],
    };
    let found = errors(&graph);
    assert!(
        found
            .iter()
            .any(|e| matches!(e, ValidationError::NodeIdMismatch { .. }))
    );
    assert!(found.iter().any(|e| matches!(e, ValidationError::NoEntry)));

    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let mut graph = b.build();
    graph.nodes[a.index()].routing =
        Routing::next(Edge::always(ir::EdgeId::new(0), NodeId::new(9)));
    assert!(
        errors(&graph)
            .iter()
            .any(|e| matches!(e, ValidationError::UnknownTarget { .. }))
    );
}

/// A step kind that is not registered is reported when a registry is supplied.
#[test]
fn unknown_step_kinds_are_reported() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_step("a", scope, StepKindId::new(42));
    let graph = b.build();

    validate(&graph).expect("valid without a registry");
    let registry = ir::StepRegistry::new();
    assert!(
        ir::validate_with(&graph, Some(&registry))
            .expect_err("kind 42 is not registered")
            .iter()
            .any(|e| matches!(e, ValidationError::UnknownStepKind { .. }))
    );
}

/// An entry node with incoming edges is a contradiction: entries are seeded.
#[test]
fn entry_nodes_may_not_have_incoming_edges() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    b.mark_entry(a);
    b.mark_entry(c);
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::EntryHasIncoming(_)))
    );
}

/// `Guard::Always` on the only arm of a group is fine.
#[test]
fn a_single_always_arm_is_legal() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.select(a, vec![Arm::always(c)]);
    let graph = b.build();
    validate(&graph).expect("valid");
    assert_eq!(
        graph.node(a).unwrap().routing.groups[0].arms[0].guard,
        Guard::Always
    );
}

/// Invariant 8: a node with an incoming back edge is a loop head, and must join
/// with `Any`.
#[test]
fn a_loop_head_must_join_with_any() {
    let build = |join: JoinPolicy| {
        let mut b = GraphBuilder::new();
        let scope = ScopeId::new(0);
        let head = b.add_step("head", scope, NOOP);
        let tail = b.add_step("tail", scope, NOOP);
        b.set_join(head, join);
        b.link(head, tail);
        b.select(tail, vec![Arm::always(head).as_back()]);
        b.set_budget(head, Budget::looped(5));
        b.set_budget(tail, Budget::looped(5));
        b.mark_entry(head);
        b.build()
    };

    for join in [JoinPolicy::All, JoinPolicy::Quorum { n: 2 }] {
        let graph = build(join);
        assert!(
            errors(&graph)
                .iter()
                .any(|e| matches!(e, ValidationError::LoopHeadMustJoinAny(_))),
            "{join:?} should be rejected on a loop head"
        );
    }
    validate(&build(JoinPolicy::Any)).expect("Any is the only legal loop-head join");
}

/// The corollary: a node cannot be both a multi-branch `All` join and a loop head.
/// The fix is a dedicated join node in front of the head.
#[test]
fn a_multi_branch_join_needs_its_own_node_in_front_of_a_loop_head() {
    // Broken: `head` tries to be both the `All` join for two branches and the target
    // of the back edge.
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    let head = b.add_step("head", scope, NOOP);
    b.fan_out(start, &[left, right]);
    b.link(left, head);
    b.link(right, head);
    b.set_join(head, JoinPolicy::All);
    b.select(head, vec![Arm::always(head).as_back()]);
    b.set_budget(head, Budget::looped(5));
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::LoopHeadMustJoinAny(_)))
    );

    // Fixed: `gate` does the `All` join, `head` does the looping.
    let mut b = GraphBuilder::new();
    let start = b.add_step("start", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    let gate = b.add_step("gate", scope, NOOP);
    let head = b.add_step("head", scope, NOOP);
    b.fan_out(start, &[left, right]);
    b.link(left, gate);
    b.link(right, gate);
    b.set_join(gate, JoinPolicy::All);
    b.link(gate, head);
    b.set_join(head, JoinPolicy::Any);
    b.select(head, vec![Arm::always(head).as_back()]);
    b.set_budget(head, Budget::looped(5));
    validate(&b.build()).expect("valid");
}

/// A scope a path can leave and return to gets a warning, not an error: release is
/// irreversible, so re-entry acquires a fresh runtime and workspace.
#[test]
fn re_entering_a_scope_is_a_warning() {
    let mut b = GraphBuilder::bare();
    let job = b.add_scope(Scope::new(ScopeId::new(0)));
    let helper = b.add_scope(Scope::new(ScopeId::new(0)));
    let first = b.add_step("first", job, NOOP);
    let away = b.add_step("away", helper, NOOP);
    let back = b.add_step("back", job, NOOP);
    b.link(first, away);
    b.link(away, back);
    let graph = b.build();

    let report = ir::check(&graph);
    assert!(report.is_ok(), "re-entry is legal, just risky");
    assert_eq!(
        report.warnings,
        vec![ValidationWarning::ScopeReentry {
            scope: job,
            via: away
        }]
    );

    // A scope nothing leaves and returns to draws no warning.
    let mut b = GraphBuilder::bare();
    let job = b.add_scope(Scope::new(ScopeId::new(0)));
    let helper = b.add_scope(Scope::new(ScopeId::new(0)));
    let first = b.add_step("first", job, NOOP);
    let second = b.add_step("second", job, NOOP);
    let after = b.add_step("after", helper, NOOP);
    b.link(first, second);
    b.link(second, after);
    assert!(ir::check(&b.build()).warnings.is_empty());
}

/// `check` reports errors and warnings together; `validate` is the errors-only view.
#[test]
fn check_reports_both_errors_and_warnings() {
    let mut b = GraphBuilder::bare();
    let job = b.add_scope(Scope::new(ScopeId::new(0)));
    let helper = b.add_scope(Scope::new(ScopeId::new(0)));
    let first = b.add_step("first", job, NOOP);
    let away = b.add_step("away", helper, NOOP);
    let back = b.add_step("back", job, NOOP);
    b.link(first, away);
    b.link(away, back);
    b.set_budget(back, Budget::new(0, Duration::from_secs(1)));
    let graph = b.build();

    let report = ir::check(&graph);
    assert!(!report.is_ok());
    assert!(!report.warnings.is_empty());
    assert!(report.into_result().is_err());
}

/// An unresolved placeholder is reported with its path, so a deep config says where.
#[test]
fn placeholder_paths_point_at_the_offending_field() {
    use ir::validate::{contains_placeholder, placeholder_path};

    let config = json!({ "env": { "TOKEN": { EXPR_PLACEHOLDER_KEY: 3 } } });
    assert!(contains_placeholder(&config));
    assert_eq!(placeholder_path(&config).as_deref(), Some("env.TOKEN"));

    let in_array = json!({ "args": ["--flag", { EXPR_PLACEHOLDER_KEY: 1 }] });
    assert_eq!(placeholder_path(&in_array).as_deref(), Some("args[1]"));

    assert_eq!(placeholder_path(&json!({ "plain": 1 })), None);
}
