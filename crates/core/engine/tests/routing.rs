//! §2: routing is an AND of XORs. Selection is the default; fan-out is
//! explicit.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use ir::{Arm, BinOp, Fallthrough, GraphBuilder, JoinPolicy, Outcome, RunStatus, Value, validate};
use support::{Harness, NOOP, registry};

/// One group, one `Always` arm: the plain sequential case.
#[test]
fn unconditional_next_runs_the_successor() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    let graph = b.build();

    ir::validate_with(&graph, Some(&registry())).expect("valid");
    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.started, vec!["a", "c"]);
}

/// One group with guarded arms: the first passing guard wins, and only one
/// token leaves the node.
#[test]
fn select_takes_the_first_matching_arm() {
    for (value, expected) in [(1, "low"), (5, "mid"), (99, "high")] {
        let mut b = GraphBuilder::new();
        let scope = ir::ScopeId::new(0);
        let pick = b.add_step("pick", scope, NOOP);
        let low = b.add_step("low", scope, NOOP);
        let mid = b.add_step("mid", scope, NOOP);
        let high = b.add_step("high", scope, NOOP);

        let (lt3, lt10) = {
            let e = b.exprs();
            let output = e.var("output");
            let three = e.lit(3);
            let lt3 = e.binary(BinOp::Lt, output, three);
            let output = e.var("output");
            let ten = e.lit(10);
            let lt10 = e.binary(BinOp::Lt, output, ten);
            (lt3, lt10)
        };
        b.select(pick, vec![
            Arm::when(low, lt3),
            Arm::when(mid, lt10),
            Arm::always(high),
        ]);
        let graph = b.build();
        validate(&graph).expect("valid");

        let mut h = Harness::new(graph).respond_with(move |info| {
            if info.base == "pick" {
                Outcome::success(value)
            } else {
                Outcome::success(Value::Null)
            }
        });
        assert_eq!(h.run(), RunStatus::Success);
        assert_eq!(h.started, vec!["pick", expected], "value {value}");
    }
}

/// Several groups: an explicit AND-split. Each group emits its own token.
#[test]
fn fan_out_needs_one_group_per_branch() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    let join = b.add_step("join", scope, NOOP);
    b.fan_out(start, &[left, right]);
    b.link(left, join);
    b.link(right, join);
    b.set_join(join, JoinPolicy::All);
    let graph = b.build();
    validate(&graph).expect("valid");

    assert_eq!(
        graph.node(start).unwrap().routing.groups.len(),
        2,
        "fan-out is two groups, not two arms of one group"
    );

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.max_concurrent, 2, "both branches run at once");
    assert_eq!(h.start_count("join"), 1);
}

/// Putting both branches in one group is selection, not fan-out: only one runs.
#[test]
fn one_group_with_two_arms_is_selection_not_fan_out() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    let yes = b.exprs().lit(true);
    b.select(start, vec![Arm::when(left, yes), Arm::always(right)]);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.started, vec!["start", "left"]);
}

/// A group whose arms all fail is an OR-split: it emits nothing.
#[test]
fn conditional_fan_out_drops_groups_that_match_nothing() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let always_runs = b.add_step("always_runs", scope, NOOP);
    let never_runs = b.add_step("never_runs", scope, NOOP);
    let no = b.exprs().lit(false);
    b.fan_out_groups(start, vec![vec![Arm::always(always_runs)], vec![
        Arm::when(never_runs, no),
    ]]);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.started, vec!["start", "always_runs"]);
}

/// `Fallthrough::Error` is for frontends that require totality.
#[test]
fn fallthrough_error_fails_the_run_when_no_arm_matches() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let never = b.add_step("never", scope, NOOP);
    let no = b.exprs().lit(false);
    b.select_with(start, vec![Arm::when(never, no)], Fallthrough::Error);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Failed);
    assert!(matches!(
        h.state.errors().first(),
        Some(engine::RunError::NoArmMatched { .. })
    ));
}

/// An edge's `map` decides the payload; without one the source output flows on.
#[test]
fn map_rewrites_the_token_payload() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    let wrapped = {
        let e = b.exprs();
        let output = e.var("output");
        e.object(vec![("wrapped", output)])
    };
    let id = b.next_edge_id();
    b.node_mut(a).routing = ir::Routing::next(ir::Edge::always(id, c).with_map(wrapped));
    let graph = b.build();
    validate(&graph).expect("valid");

    let seen = Rc::new(RefCell::new(Value::Null));
    let sink = seen.clone();
    let mut h = Harness::new(graph).respond_with(move |info| {
        if info.base == "c" {
            *sink.borrow_mut() = info.input();
        }
        Outcome::success(7)
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(*seen.borrow(), serde_json::json!({"wrapped": 7}));
}

/// A precondition that is false skips the node, and routing still runs so
/// `always()` downstream still sees it.
#[test]
fn false_precondition_skips_but_still_routes() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let gated = b.add_step("gated", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    let no = b.exprs().lit(false);
    b.set_precondition(gated, no);
    b.link(a, gated);
    b.link(gated, after);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.started, vec!["a", "after"], "gated never executed");
    assert_eq!(h.status_of("gated").as_deref(), Some("skipped"));
}

/// `failure()` guards route around a failed step; `always()` runs regardless.
#[test]
fn status_functions_drive_recovery_paths() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let build = b.add_step("build", scope, NOOP);
    let notify = b.add_step("notify", scope, NOOP);
    let deploy = b.add_step("deploy", scope, NOOP);
    let (failed, ok) = {
        let e = b.exprs();
        (e.call("failure", vec![]), e.call("success", vec![]))
    };
    b.fan_out_groups(build, vec![vec![Arm::when(notify, failed)], vec![
        Arm::when(deploy, ok),
    ]]);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| {
        if info.base == "build" {
            Outcome::failure("compile error")
        } else {
            Outcome::success(Value::Null)
        }
    });
    assert_eq!(h.run(), RunStatus::Failed, "the run still folds to failed");
    assert_eq!(h.started, vec!["build", "notify"]);
}

/// A skip propagates: a node whose only upstream was skipped is skipped too,
/// because `success()` folds the upstream statuses.
#[test]
fn a_skip_propagates_down_the_chain() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let gated = b.add_step("gated", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    let last = b.add_step("last", scope, NOOP);
    let (no, succeeded) = {
        let e = b.exprs();
        (e.lit(false), e.call("success", vec![]))
    };
    b.set_precondition(gated, no);
    b.set_precondition(after, succeeded);
    b.link(a, gated);
    b.link(gated, after);
    b.link(after, last);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success, "a skip is not a failure");
    assert_eq!(h.status_of("gated").as_deref(), Some("skipped"));
    assert_eq!(h.status_of("after").as_deref(), Some("skipped"));
    assert_eq!(
        h.started,
        vec!["a", "last"],
        "only `last` runs after the skip"
    );
}

/// `always()` runs a node even when everything upstream was skipped.
#[test]
fn always_runs_after_a_skip() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let gated = b.add_step("gated", scope, NOOP);
    let cleanup = b.add_step("cleanup", scope, NOOP);
    let (no, whatever) = {
        let e = b.exprs();
        (e.lit(false), e.call("always", vec![]))
    };
    b.set_precondition(gated, no);
    b.set_precondition(cleanup, whatever);
    b.link(a, gated);
    b.link(gated, cleanup);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.started, vec!["a", "cleanup"]);
}
