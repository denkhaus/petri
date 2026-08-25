//! §6: parallel `for_each` splices clones into the live graph. Each clone gets
//! `item` and `index`; each adds its own edge into the collector.

mod support;

use ir::validate::EXPR_PLACEHOLDER_KEY;
use ir::{
    Arm, CancelScopeId, ExpandTarget, GraphBuilder, JoinPolicy, Outcome, RunStatus, StepRef, Value,
    collector_exprs, parallel_for_each, validate,
};
use serde_json::json;
use support::{Harness, NOOP};

/// Build: plan -> deploy (expanded per region) -> collect.
fn matrix_graph(max_parallel: Option<u32>, fail_fast: bool) -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let deploy = b.add_step("deploy", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);

    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, deploy);
    b.select(
        deploy,
        vec![Arm::always(collect).with_map(collector.indexed)],
    );
    b.select(collect, vec![]);
    b.node_mut(collect).routing = ir::Routing::terminal();
    b.set_join(collect, JoinPolicy::All);

    let item = b.exprs().var("item");
    b.node_mut(deploy).step = StepRef::new(
        NOOP,
        json!({ "region": { EXPR_PLACEHOLDER_KEY: item.raw() } }),
    );

    parallel_for_each(
        &mut b,
        deploy,
        items,
        ExpandTarget::Node,
        max_parallel,
        fail_fast,
    );
    b.build()
}

/// Every element gets a clone, and the collector's `All` join counts the edges the
/// splice added.
#[test]
fn for_each_clones_the_node_and_the_collector_waits_for_all_of_them() {
    let graph = matrix_graph(None, false);
    validate(&graph).expect("valid");
    assert!(!graph.is_plan(), "an unexpanded graph is HIR, not a plan");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["us-east", "us-west", "eu"])),
        "deploy" => Outcome::success(info.config.get("region").cloned().unwrap_or(Value::Null)),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    assert_eq!(h.start_count("deploy"), 3);
    assert_eq!(h.max_concurrent, 3, "clones run concurrently by default");
    assert_eq!(h.start_count("collect"), 1);
    assert_eq!(
        h.started,
        vec!["plan", "deploy#0", "deploy#1", "deploy#2", "collect"]
    );
}

/// `index` and `item` are bound inside each clone, so the collector can put the
/// results back in order.
#[test]
fn clones_carry_item_and_index_and_the_collector_orders_results() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let build = b.add_step("build", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let report = b.add_step("report", scope, NOOP);

    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    let item = b.exprs().var("item");
    b.link(plan, build);
    b.select(
        build,
        vec![Arm::always(collect).with_map(collector.indexed)],
    );
    b.select(
        collect,
        vec![Arm::always(report).with_map(collector.ordered)],
    );
    b.set_join(collect, JoinPolicy::All);
    b.node_mut(build).step = StepRef::new(
        NOOP,
        json!({ "target": { EXPR_PLACEHOLDER_KEY: item.raw() } }),
    );
    parallel_for_each(&mut b, build, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let seen = std::rc::Rc::new(std::cell::RefCell::new(Value::Null));
    let sink = seen.clone();
    let mut h = Harness::new(graph).respond_with(move |info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["alpha", "beta", "gamma"])),
        "build" => Outcome::success(json!(format!(
            "built {}",
            info.config
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("")
        ))),
        "report" => {
            *sink.borrow_mut() = info.input();
            Outcome::success(Value::Null)
        }
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(
        *seen.borrow(),
        json!(["built alpha", "built beta", "built gamma"]),
        "results come back in items order, not completion order"
    );
}

/// `max_parallel` is admission control across the spliced clones.
#[test]
fn max_parallel_limits_how_many_clones_run_at_once() {
    let graph = matrix_graph(Some(2), false);
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b", "c", "d", "e"])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("deploy"), 5, "every element still runs");
    assert_eq!(h.max_concurrent, 2, "never more than two at a time");
}

/// `fail_fast` cancels the sibling clones through the splice's own cancel scope.
#[test]
fn fail_fast_cancels_the_sibling_clones() {
    let graph = matrix_graph(None, true);
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b", "c"])),
        "deploy" if info.index == Some(0) => Outcome::failure("boom"),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Failed);

    let cancels = h
        .commands
        .iter()
        .filter(|c| matches!(c, engine::Command::DeliverControl { .. }))
        .count();
    assert_eq!(cancels, 2, "the two live siblings are cancelled");
    assert_eq!(h.start_count("collect"), 0, "the collector never fires");

    // The splice made its own cancel scope, nested under the root.
    let splice = h.state.splices().first().expect("one splice");
    assert_ne!(splice.cancel_scope, CancelScopeId::ROOT);
    assert_eq!(
        h.state.cancel_scope(splice.cancel_scope).unwrap().parent,
        Some(CancelScopeId::ROOT)
    );
    assert!(h.state.cancel_scope(splice.cancel_scope).unwrap().cancelled);
}

/// A subgraph expansion clones the whole region, not just one node.
#[test]
fn subgraph_expansion_clones_the_region() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let setup = b.add_step("setup", scope, NOOP);
    let test = b.add_step("test", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);

    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, setup);
    b.link(setup, test);
    b.select(test, vec![Arm::always(collect).with_map(collector.indexed)]);
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(
        &mut b,
        setup,
        items,
        ExpandTarget::Subgraph {
            entry: setup,
            exit: test,
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
    assert_eq!(h.start_count("test"), 2);
    assert_eq!(h.start_count("collect"), 1);
    assert!(
        h.state.is_superseded(setup),
        "the original region entry is replaced by its clones"
    );
}

/// Expanding over an empty array leaves the collector with nothing to wait for and
/// no clone to run.
#[test]
fn an_empty_items_array_expands_to_nothing() {
    let graph = matrix_graph(None, false);
    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!([])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("deploy"), 0);
    assert_eq!(h.start_count("collect"), 0);
    assert!(h.state.is_quiescent());
}

/// `items` that is not an array is a run error, not a panic.
#[test]
fn non_array_items_fails_the_run() {
    let graph = matrix_graph(None, false);
    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!("not an array")),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Failed);
    assert!(matches!(
        h.state.errors().first(),
        Some(engine::RunError::ItemsNotArray { .. })
    ));
}
