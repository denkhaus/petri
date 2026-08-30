//! §1: how the run's status folds from node outcomes — `Completion::AnyFailure`
//! (the CI rule, the default) versus `Completion::TerminalNode` (the fabro
//! rule: a failure that routes onward is control flow; only the exit node's
//! record decides).

mod support;

use std::collections::BTreeMap;

use engine::Event;
use ir::{
    Arm, CancelScopeId, Completion, EdgeId, FiringId, Generation, Graph, GraphBuilder, NodeId,
    Outcome, RunStatus, ScopeId, Token, Value, validate,
};
use support::{Harness, NOOP};

/// `work -> exit`, unconditionally: a failure at `work` still routes.
fn routed_failure_graph() -> (Graph, NodeId) {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let work = b.add_step("work", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    b.link(work, exit);
    let graph = b.build();
    validate(&graph).expect("valid");
    (graph, exit)
}

fn with_terminal(mut graph: Graph, exit: NodeId) -> Graph {
    graph.completion = Completion::TerminalNode(exit);
    validate(&graph).expect("valid");
    graph
}

/// A token on an edge no graph declares: the cheapest way to plant a `RunError`
/// mid-run without touching any step.
fn bad_token() -> Event {
    Event::TokenEmitted(Token::new(
        EdgeId::new(999),
        Generation::ZERO,
        Value::Null,
        FiringId::new(0),
    ))
}

/// The same graph, both policies: a failed node that routes onward is control
/// flow under `TerminalNode`, and a run failure under `AnyFailure`.
#[test]
fn a_routed_failure_is_control_flow_under_terminal_node() {
    let results = BTreeMap::from([("work", Outcome::failure("boom"))]);

    let (graph, exit) = routed_failure_graph();
    let mut any = Harness::new(graph.clone()).results(results.clone());
    assert_eq!(any.run(), RunStatus::Failed);

    let mut terminal = Harness::new(with_terminal(graph, exit)).results(results);
    assert_eq!(terminal.run(), RunStatus::Success);
    assert_eq!(terminal.status_of("work").as_deref(), Some("failure"));
    assert_eq!(terminal.status_of("exit").as_deref(), Some("success"));
}

/// A failure on a node that tolerates it is control flow under `AnyFailure`:
/// the run folds to `Success`, while the record itself still says `failure` —
/// guards and status reads see it unchanged.
#[test]
fn a_tolerated_failure_does_not_fail_the_run() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let work = b.add_step("work", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    b.node_mut(work).tolerates_failure = true;
    b.link(work, exit);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h =
        Harness::new(graph).results(BTreeMap::from([("work", Outcome::failure("tolerated"))]));
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.status_of("work").as_deref(), Some("failure"));
    assert_eq!(h.status_of("exit").as_deref(), Some("success"));
    h.verify_replay();
}

/// Quiescence without the exit record is `Failed` — including the successful
/// dead end, where every node that ran succeeded. This is a deliberate
/// departure from fabro_core, whose executor reports success when traversal
/// stops on a succeeded node with no outgoing edge; the fabro frontend rejects
/// that shape at load time instead (resolved decision 3).
#[test]
fn a_successful_dead_end_without_the_terminal_record_fails() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    // `start` succeeds and routes nowhere; `exit` is never reached.
    b.mark_entry(start);
    let graph = with_terminal(b.build(), exit);

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.status_of("start").as_deref(), Some("success"));
    assert_eq!(h.status_of("exit"), None, "the exit never recorded");
}

/// The exit was reached but its own step failed: `Failed`.
#[test]
fn a_failed_terminal_node_fails_the_run() {
    let (graph, exit) = routed_failure_graph();
    let mut h = Harness::new(with_terminal(graph, exit))
        .results(BTreeMap::from([("exit", Outcome::failure("no good"))]));
    assert_eq!(h.run(), RunStatus::Failed);
}

/// A `RunError` fails the run under both policies: engine errors are never
/// control flow, however healthy the exit record looks.
#[test]
fn a_run_error_fails_the_run_under_both_policies() {
    for terminal in [false, true] {
        let (graph, exit) = routed_failure_graph();
        let graph = if terminal {
            with_terminal(graph, exit)
        } else {
            graph
        };
        let mut h = Harness::new(graph);
        h.feed(Event::RunStarted);
        let starts = h.take_starts();
        h.feed(bad_token());
        h.finish(starts[0].0, Outcome::success(Value::Null));
        let exits = h.take_starts();
        h.finish(exits[0].0, Outcome::success(Value::Null));

        assert!(h.state.is_finished());
        assert!(!h.state.errors().is_empty());
        assert_eq!(
            h.state.folded_status(),
            RunStatus::Failed,
            "terminal={terminal}"
        );
    }
}

/// A root cancel folds to `Cancelled` under both policies.
#[test]
fn a_root_cancel_folds_to_cancelled_under_both_policies() {
    for terminal in [false, true] {
        let (graph, exit) = routed_failure_graph();
        let graph = if terminal {
            with_terminal(graph, exit)
        } else {
            graph
        };
        let mut h = Harness::new(graph);
        h.feed(Event::RunStarted);
        let starts = h.take_starts();
        h.cancel(CancelScopeId::ROOT);
        h.finish(starts[0].0, Outcome::cancelled());

        assert!(h.state.is_finished());
        assert_eq!(
            h.state.folded_status(),
            RunStatus::Cancelled,
            "terminal={terminal}"
        );
    }
}

/// A `RunError` sets `run.failed` for later guards: `has_any_failure` includes
/// the error list, not just failed history records.
#[test]
fn a_run_error_sets_run_failed_for_later_guards() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let check = b.add_step("check", scope, NOOP);
    let flag = b.add_step("flag", scope, NOOP);
    let failed = b.exprs().path("run", &["failed"]);
    b.link(start, check);
    b.select(check, vec![Arm::when(flag, failed)]);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    h.feed(bad_token());
    h.finish(starts[0].0, Outcome::success(Value::Null));
    let checks = h.take_starts();
    h.finish(checks[0].0, Outcome::success(Value::Null));

    let flags = h.take_starts();
    assert_eq!(flags.len(), 1, "the guard saw run.failed");
    assert_eq!(flags[0].1, "flag");
    h.finish(flags[0].0, Outcome::success(Value::Null));
    assert!(h.state.is_finished());
}

/// Under `TerminalNode`, a healthy run's `run.failed` guard reads false
/// mid-run. The regression this pins: folding the status for the guard would
/// read `Failed` until the exit record exists, poisoning every `run.failed`
/// guard on the way.
#[test]
fn run_failed_stays_false_mid_run_under_terminal_node() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let gate = b.add_step("gate", scope, NOOP);
    let bad = b.add_step("bad", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    let failed = b.exprs().path("run", &["failed"]);
    b.link(start, gate);
    b.select(gate, vec![Arm::when(bad, failed), Arm::always(exit)]);
    let graph = with_terminal(b.build(), exit);

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert!(h.started.iter().all(|n| n != "bad"), "{:?}", h.started);
    assert_eq!(h.status_of("exit").as_deref(), Some("success"));
    h.verify_replay();
}

/// And after a real failure, `run.failed` still reflects any-failure mid-run
/// under `TerminalNode` — while the run itself still folds to `Success` once
/// the exit completes, because that failure is control flow.
#[test]
fn run_failed_still_reflects_any_failure_under_terminal_node() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let work = b.add_step("work", scope, NOOP);
    let gate = b.add_step("gate", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    let failed = b.exprs().path("run", &["failed"]);
    b.link(work, gate);
    b.select(gate, vec![Arm::when(exit, failed)]);
    let graph = with_terminal(b.build(), exit);

    let mut h =
        Harness::new(graph).results(BTreeMap::from([("work", Outcome::failure("expected"))]));
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(
        h.status_of("exit").as_deref(),
        Some("success"),
        "the guard routed to the exit off the recorded failure"
    );
    h.verify_replay();
}

/// The determinism canary on a `TerminalNode` run: replay is byte-identical.
#[test]
fn replay_is_byte_identical_under_terminal_node() {
    let (graph, exit) = routed_failure_graph();
    let mut h = Harness::new(with_terminal(graph, exit))
        .results(BTreeMap::from([("work", Outcome::failure("boom"))]));
    assert_eq!(h.run(), RunStatus::Success);
    h.verify_replay();
}
