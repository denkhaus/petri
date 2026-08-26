//! §5a: cancel scopes. A scope is a dynamic set of firings cancellable as a unit.
//! Cancellation is a state transition in the pure core; only signal delivery is
//! the host's job.

mod support;

use engine::{Command, Event};
use ir::{
    CancelScopeId, Control, ExpandTarget, GraphBuilder, JoinPolicy, Outcome, RunStatus, Value,
    collector_exprs, parallel_for_each, validate,
};
use serde_json::json;
use support::{Harness, NOOP};

/// Cancelling the root scope cancels every live firing and drops every token.
#[test]
fn cancelling_the_root_scope_stops_the_run() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.fan_out(start, &[left, right]);
    b.link(left, after);
    b.link(right, after);
    b.set_join(after, JoinPolicy::All);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(Value::Null));

    let branches = h.take_starts();
    assert_eq!(branches.len(), 2);

    h.cancel(CancelScopeId::ROOT);
    let delivered: Vec<_> = h
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::DeliverControl { firing, ctl } => Some((*firing, ctl.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(delivered.len(), 2, "both live firings get the signal");
    assert!(delivered.iter().all(|(_, ctl)| *ctl == Control::Cancel));
    assert!(h.state.is_cancelled());

    // The host reports back; the run ends cancelled and `after` never runs.
    for (firing, _) in branches {
        h.finish(firing, Outcome::cancelled());
    }
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    assert_eq!(h.start_count("after"), 0);
    assert_eq!(h.state.pending_count(), 0);
}

/// A cancelled firing does not route: its tokens would restart the work the
/// cancellation was meant to stop.
#[test]
fn a_cancelled_firing_does_not_route() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.finish(starts[0].0, Outcome::cancelled());

    assert_eq!(h.start_count("c"), 0);
    assert_eq!(h.status, Some(RunStatus::Cancelled));
}

/// Splice scopes nest under the root, so cancelling the root reaches inside them.
#[test]
fn cancelling_the_root_reaches_into_splice_scopes() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let work = b.add_step("work", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, work);
    b.select(
        work,
        vec![ir::Arm::always(collect).with_map(collector.indexed)],
    );
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(&mut b, work, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(json!(["a", "b", "c"])));

    let clones = h.take_starts();
    assert_eq!(clones.len(), 3);

    let splice_scope = h.state.splices().first().unwrap().cancel_scope;
    h.cancel(CancelScopeId::ROOT);
    assert!(
        h.state.cancel_scope(splice_scope).unwrap().cancelled,
        "the nested splice scope is cancelled too"
    );
    let delivered = h
        .commands
        .iter()
        .filter(|c| matches!(c, Command::DeliverControl { .. }))
        .count();
    assert_eq!(delivered, 3);
}

/// Cancelling one splice scope leaves work outside it alone.
#[test]
fn cancelling_a_splice_scope_spares_the_rest_of_the_run() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let matrix = b.add_step("matrix", scope, NOOP);
    let sibling = b.add_step("sibling", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.fan_out(plan, &[matrix, sibling]);
    b.select(
        matrix,
        vec![ir::Arm::always(collect).with_map(collector.indexed)],
    );
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(&mut b, matrix, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(json!(["a", "b"])));

    let running = h.take_starts();
    // Two clones plus the sibling branch.
    assert_eq!(running.len(), 3);

    let splice_scope = h.state.splices().first().unwrap().cancel_scope;
    h.cancel(splice_scope);
    let delivered = h
        .commands
        .iter()
        .filter(|c| matches!(c, Command::DeliverControl { .. }))
        .count();
    assert_eq!(delivered, 2, "only the clones are cancelled");
    assert!(!h.state.is_cancelled(), "the run itself is not cancelled");

    for (firing, name) in running {
        let outcome = if name.starts_with("matrix") {
            Outcome::cancelled()
        } else {
            Outcome::success(Value::Null)
        };
        h.finish(firing, outcome);
    }
    assert_eq!(h.status, Some(RunStatus::Success));
    assert_eq!(h.start_count("collect"), 0);
}

/// Cancelling twice does not deliver the signal twice.
#[test]
fn cancelling_twice_delivers_one_signal() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let graph = b.build();
    validate(&graph).expect("valid");
    let _ = a;

    let mut h = Harness::new(graph);
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.cancel(CancelScopeId::ROOT);
    let delivered = h
        .commands
        .iter()
        .filter(|c| matches!(c, Command::DeliverControl { .. }))
        .count();
    assert_eq!(delivered, 1);
    h.finish(starts[0].0, Outcome::cancelled());
    assert_eq!(h.status, Some(RunStatus::Cancelled));
}
