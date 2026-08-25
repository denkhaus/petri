//! §6, "GHA frontend mapping". GitHub Actions lowers onto the degenerate subset:
//! no back edges, no `Any` or `Quorum` joins, no multi-arm groups.

mod support;

use ir::{
    Arm, ExpandTarget, Graph, GraphBuilder, JoinPolicy, NodeId, Outcome, RunStatus, Scope, ScopeId,
    Value, collector_exprs, parallel_for_each, validate,
};
use serde_json::json;
use support::{Harness, NOOP};

/// ```yaml
/// jobs:
///   build:   { steps: [checkout, compile] }
///   test:    { needs: build, strategy: { matrix: { suite: [unit, integration] } } }
///   lint:    { needs: build }
///   publish: { needs: [test, lint], if: success() }
/// ```
fn workflow() -> (Graph, NodeId) {
    let mut b = GraphBuilder::bare();
    let build = b.add_scope(Scope::new(ScopeId::new(0)));
    let test = b.add_scope(Scope::new(ScopeId::new(0)));
    let lint = b.add_scope(Scope::new(ScopeId::new(0)));
    let release = b.add_scope(Scope::new(ScopeId::new(0)));

    // A job is a scope plus a chain of step nodes.
    let checkout = b.add_step("checkout", build, NOOP);
    let compile = b.add_step("compile", build, NOOP);
    let setup = b.add_step("setup", test, NOOP);
    let run = b.add_step("run", test, NOOP);
    let lint_step = b.add_step("lint", lint, NOOP);
    let publish = b.add_step("publish", release, NOOP);

    let collector = collector_exprs(b.exprs());

    // steps (sequential) -> single-group Always edges.
    b.link(checkout, compile);
    b.link(setup, run);

    // A job with k dependents -> k groups of one arm: explicit fan-out.
    b.fan_out(compile, &[setup, lint_step]);

    b.select(run, vec![Arm::always(publish).with_map(collector.indexed)]);
    b.link(lint_step, publish);

    // needs: [test, lint] -> JoinPolicy::All on the dependent's entry node.
    b.set_join(publish, JoinPolicy::All);

    // if: success() -> precondition.
    let succeeded = b.exprs().call("success", vec![]);
    b.set_precondition(publish, succeeded);

    // strategy.matrix -> Expansion::ForEach over the job's subgraph.
    let suites = b.exprs().lit(json!(["unit", "integration"]));
    parallel_for_each(
        &mut b,
        setup,
        suites,
        ExpandTarget::Subgraph {
            entry: setup,
            exit: run,
        },
        None,
        true,
    );

    let graph = b.build();
    (graph, publish)
}

/// The whole mapping runs end to end.
#[test]
fn a_gha_workflow_runs() {
    let (graph, _) = workflow();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("setup"), 2, "one matrix leg per suite");
    assert_eq!(h.start_count("run"), 2);
    assert_eq!(h.start_count("lint"), 1);
    assert_eq!(h.start_count("publish"), 1, "publish waits for both jobs");
    assert_eq!(h.started.first().map(String::as_str), Some("checkout"));
    assert_eq!(h.started.last().map(String::as_str), Some("publish"));
}

/// GHA exercises only the degenerate subset of the IR, by construction.
#[test]
fn gha_lowering_uses_only_the_degenerate_subset() {
    let (graph, _) = workflow();
    for node in &graph.nodes {
        assert_eq!(
            node.join,
            JoinPolicy::All,
            "{} should join with All",
            node.name
        );
        for group in &node.routing.groups {
            assert_eq!(
                group.arms.len(),
                1,
                "{} has a multi-arm group; GHA never produces one",
                node.name
            );
        }
    }
    assert!(
        !graph.edges().any(|e| e.back),
        "GHA lowering never produces back edges"
    );
}

/// A failed job skips its dependents through `if: success()`, and the skip
/// propagates rather than hanging the join.
#[test]
fn a_failed_job_skips_its_dependents() {
    let (graph, _) = workflow();
    let mut h = Harness::new(graph).respond_with(|info| {
        if info.base == "lint" {
            Outcome::failure("style")
        } else {
            Outcome::success(Value::Null)
        }
    });
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.start_count("publish"), 0, "publish never executes");
    assert_eq!(h.status_of("publish").as_deref(), Some("skipped"));
}

/// `fail-fast` on a matrix maps straight onto the splice's cancel scope.
#[test]
fn matrix_fail_fast_cancels_the_other_legs() {
    let (graph, _) = workflow();
    let mut h = Harness::new(graph).respond_with(|info| {
        if info.base == "run" && info.index == Some(0) {
            Outcome::failure("test failure")
        } else {
            Outcome::success(Value::Null)
        }
    });
    assert_eq!(h.run(), RunStatus::Failed);
    assert!(
        h.commands
            .iter()
            .any(|c| matches!(c, engine::Command::DeliverControl { .. })),
        "the surviving leg is cancelled"
    );
    assert_eq!(h.start_count("publish"), 0);
}
