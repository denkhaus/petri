//! Outcome-driven splice, the Append path: prepare-then-commit finalization,
//! policy, attachment, ordered composition, and atomic rejection. Named for the
//! rules they pin.

mod support;

use std::collections::BTreeMap;

use engine::INVALID_SPLICE_CLASS;
use ir::{
    Attachment, Edge, EdgeId, ExistingNodeRef, Graph, GraphBuilder, GraphFragment, JoinPolicy,
    Local, Node, NodeId, Outcome, ReplaceScope, RunStatus, Scope, ScopeId, SplicePolicy,
    SpliceRequest, StepRef, Value,
};
use support::{Harness, NOOP};

/// A linear fragment: `names` chained by unconditional edges, entry at the
/// first, exit at the last, one declared scope.
fn fragment(names: &[&str]) -> GraphFragment {
    let mut nodes: Vec<Node<Local>> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            Node::new(
                NodeId::new(i as u32),
                name,
                ScopeId::new(0),
                StepRef::new("noop", Value::Null),
            )
        })
        .collect();
    for i in 0..nodes.len().saturating_sub(1) {
        let edge = Edge::always(EdgeId::new(i as u32), NodeId::new(i as u32 + 1));
        nodes[i].routing = ir::Routing::next(edge);
    }
    GraphFragment {
        entries: vec![NodeId::new(0)],
        exits: vec![NodeId::new(nodes.len() as u32 - 1)],
        nodes,
        scopes: vec![Scope::new(ScopeId::new(0))],
        exprs: Default::default(),
    }
}

/// `up -> down`, with `up` granted `policy`.
fn uploader_graph(policy: SplicePolicy) -> Graph {
    let mut b = GraphBuilder::new();
    let up = b.add_step("up", ScopeId::new(0), NOOP);
    let down = b.add_step("down", ScopeId::new(0), NOOP);
    b.link(up, down);
    b.node_mut(up).splice_policy = policy;
    b.build()
}

fn splice_outcome(requests: Vec<SpliceRequest>) -> Outcome {
    Outcome::success(Value::Null).with_splices(requests)
}

// ── The Append path ───────────────────────────────────────────────────────

#[test]
fn an_appended_fragment_runs_and_the_dependent_waits_for_the_batch() {
    let graph = uploader_graph(SplicePolicy::Append);
    let mut h = Harness::new(graph.clone()).results(BTreeMap::from([(
        "up",
        splice_outcome(vec![SpliceRequest::append(fragment(&["a", "b"]))]),
    )]));
    assert_eq!(h.run(), RunStatus::Success);

    // The fragment ran, in order, and `down` — an existing dependent of the
    // uploader — waited for the batch exit as well as the uploader.
    assert_eq!(h.started, vec!["up", "a", "b", "down"]);
    assert_eq!(h.status_of("a").as_deref(), Some("success"));
    assert_eq!(h.status_of("b").as_deref(), Some("success"));
    assert!(h.state.errors().is_empty(), "{:?}", h.state.errors());

    // One batch record, owned by the uploader, with its own cancel scope.
    let batch = h.state.splices().first().expect("one batch");
    assert_eq!(batch.owner, NodeId::new(0));
    assert_eq!(batch.nodes.len(), 2);
    h.verify_replay();
}

#[test]
fn a_failed_uploader_applies_the_fragment_but_starts_none_of_it() {
    // The entry guard is success-like: a failing uploader's batch lands in the
    // graph but never starts, and the run fails on the uploader's own record.
    let graph = uploader_graph(SplicePolicy::Append);
    let mut h = Harness::new(graph.clone()).results(BTreeMap::from([(
        "up",
        Outcome::failure("boom").with_splice(SpliceRequest::append(fragment(&["a"]))),
    )]));
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.start_count("a"), 0);
    assert!(h.state.graph.nodes.iter().any(|n| n.name == "a"));
    h.verify_replay();
}

// ── Policy ────────────────────────────────────────────────────────────────

#[test]
fn the_deny_default_rejects_and_routes_invalid_splice() {
    let graph = uploader_graph(SplicePolicy::Deny);
    let before = graph.nodes.len();
    let mut h = Harness::new(graph.clone()).results(BTreeMap::from([(
        "up",
        splice_outcome(vec![SpliceRequest::append(fragment(&["a"]))]),
    )]));
    assert_eq!(h.run(), RunStatus::Failed);

    // The rejection is a routable node failure, never a run abort: no RunError,
    // the record carries the canonical class, and routing still ran `down`.
    assert!(h.state.errors().is_empty(), "{:?}", h.state.errors());
    let record = h.state.history().first().expect("up's record");
    let info = record.outcome.status.failure_info().expect("a failure");
    assert_eq!(info.class, INVALID_SPLICE_CLASS);
    assert!(info.message.contains("request 0"), "{}", info.message);
    assert_eq!(h.state.graph.nodes.len(), before, "no fragment applied");
    assert_eq!(h.start_count("down"), 1);
    h.verify_replay();
}

#[test]
fn the_cap_blocks_minting_and_equal_authority_chaining_works() {
    // Minting: a fragment node declaring more authority than its uploader.
    let graph = uploader_graph(SplicePolicy::Append);
    let mut minted = fragment(&["a"]);
    minted.nodes[0].splice_policy = SplicePolicy::Replace {
        scope: ReplaceScope::AllPending,
    };
    let mut h = Harness::new(graph.clone()).results(BTreeMap::from([(
        "up",
        splice_outcome(vec![SpliceRequest::append(minted)]),
    )]));
    assert_eq!(h.run(), RunStatus::Failed);
    let info = h.state.history()[0]
        .outcome
        .status
        .failure_info()
        .expect("a failure");
    assert_eq!(info.class, INVALID_SPLICE_CLASS);
    h.verify_replay();

    // Chaining: equal authority delegates, and the delegated node uploads too.
    let graph = uploader_graph(SplicePolicy::Append);
    let mut chained = fragment(&["a"]);
    chained.nodes[0].splice_policy = SplicePolicy::Append;
    let mut h = Harness::new(graph.clone()).results(BTreeMap::from([
        (
            "up",
            splice_outcome(vec![SpliceRequest::append(chained.clone())]),
        ),
        (
            "a",
            splice_outcome(vec![SpliceRequest::append(fragment(&["a2"]))]),
        ),
    ]));
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("a2"), 1);
    h.verify_replay();
}

// ── Atomicity and ordering ────────────────────────────────────────────────

#[test]
fn a_failing_second_request_leaves_no_fragment_and_no_context_updates() {
    let graph = uploader_graph(SplicePolicy::Append);
    let before = graph.nodes.len();
    // Request 0 is fine; request 1 collides with the live name `down`.
    let outcome = Outcome::success(Value::Null)
        .with_context_update("left_behind", "yes")
        .with_splices(vec![
            SpliceRequest::append(fragment(&["a"])),
            SpliceRequest::append(fragment(&["down"])),
        ]);
    let mut h = Harness::new(graph.clone()).results(BTreeMap::from([("up", outcome)]));
    assert_eq!(h.run(), RunStatus::Failed);

    assert_eq!(h.state.graph.nodes.len(), before, "nothing partial commits");
    assert!(
        h.state.run_context().get("left_behind").is_none(),
        "a rejected splice drops the outcome's context updates"
    );
    let info = h.state.history()[0]
        .outcome
        .status
        .failure_info()
        .expect("a failure");
    assert_eq!(info.class, INVALID_SPLICE_CLASS);
    assert!(info.message.contains("request 1"), "{}", info.message);
    h.verify_replay();
}

#[test]
fn a_later_request_may_reference_an_earlier_ones_node_but_not_the_reverse() {
    // Forward: request 1's `y` depends on request 0's `x`.
    let graph = uploader_graph(SplicePolicy::Append);
    let depends = SpliceRequest::append(fragment(&["y"])).with_attachment(Attachment::DependsOn {
        node: NodeId::new(0),
        on: ExistingNodeRef::new("x"),
    });
    let mut h = Harness::new(graph.clone()).results(BTreeMap::from([(
        "up",
        splice_outcome(vec![SpliceRequest::append(fragment(&["x"])), depends.clone()]),
    )]));
    assert_eq!(h.run(), RunStatus::Success);
    let x_started = h.started.iter().position(|n| n == "x").expect("x started");
    let y_started = h.started.iter().position(|n| n == "y").expect("y started");
    assert!(x_started < y_started, "y waits for x: {:?}", h.started);
    h.verify_replay();

    // Reversed: the reference comes before the node exists — atomic rejection.
    let graph = uploader_graph(SplicePolicy::Append);
    let before = graph.nodes.len();
    let mut h = Harness::new(graph.clone()).results(BTreeMap::from([(
        "up",
        splice_outcome(vec![depends, SpliceRequest::append(fragment(&["x"]))]),
    )]));
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.state.graph.nodes.len(), before);
    h.verify_replay();
}

// ── Retry classification and final-attempt-only ───────────────────────────

#[test]
fn invalid_splice_is_an_ordinary_retry_class_and_a_later_attempt_can_succeed() {
    let mut graph = uploader_graph(SplicePolicy::Append);
    graph.nodes[0].retry =
        ir::RetryPolicy::attempts(2).with_retry_on(ir::RetryOn::classes(&[INVALID_SPLICE_CLASS]));
    let before = graph.nodes.len();

    let mut h = Harness::new(graph.clone()).respond_with(move |info| {
        if info.base != "up" {
            return Outcome::success(Value::Null);
        }
        if info.attempt.raw() == 1 {
            // Denied by the *shape*, not the policy: an empty Append rejects.
            splice_outcome(vec![SpliceRequest::append(GraphFragment::new())])
        } else {
            splice_outcome(vec![SpliceRequest::append(fragment(&["a"]))])
        }
    });
    assert_eq!(h.run(), RunStatus::Success);

    // One retry was scheduled off the converted class; the first attempt's
    // requests are in the log and not in the graph.
    assert_eq!(h.scheduled_retries.len(), 1);
    assert_eq!(h.start_count("up"), 2);
    assert_eq!(h.start_count("a"), 1);
    assert_eq!(
        h.state.graph.nodes.len(),
        before + 1,
        "only the final attempt's fragment applied"
    );
    let record = h
        .state
        .history()
        .iter()
        .find(|r| r.name == "up")
        .expect("one final record");
    assert_eq!(record.attempt.raw(), 2);
    assert!(record.outcome.status.is_success_like());
    h.verify_replay();
}

#[test]
fn a_matching_retry_leaves_graph_and_context_unchanged() {
    let mut graph = uploader_graph(SplicePolicy::Append);
    graph.nodes[0].retry =
        ir::RetryPolicy::attempts(2).with_retry_on(ir::RetryOn::classes(&[INVALID_SPLICE_CLASS]));
    let before = graph.nodes.len();

    let mut h = Harness::new(graph.clone());
    h.feed(engine::Event::RunStarted);
    let starts = h.take_starts();
    let (firing, _) = starts.first().expect("up started");
    let bad = Outcome::success(Value::Null)
        .with_context_update("poison", true)
        .with_splice(SpliceRequest::append(GraphFragment::new()));
    h.finish(*firing, bad);

    // Mid-backoff: nothing recorded, nothing merged, nothing spliced.
    assert_eq!(h.state.graph.nodes.len(), before);
    assert!(h.state.run_context().get("poison").is_none());
    assert!(h.state.history().is_empty());
    assert_eq!(h.state.live_firings().count(), 1, "the firing stays live");
}

// ── Attachment ────────────────────────────────────────────────────────────

#[test]
fn a_not_final_reference_gets_a_real_edge_and_emits_later() {
    // Two entries: the uploader, and `slow`, which finishes after the splice.
    let mut b = GraphBuilder::new();
    let up = b.add_step("up", ScopeId::new(0), NOOP);
    let _slow = b.add_step("slow", ScopeId::new(0), NOOP);
    b.node_mut(up).splice_policy = SplicePolicy::Append;
    let graph = b.build();

    let request = SpliceRequest::append(fragment(&["f"])).with_attachment(Attachment::DependsOn {
        node: NodeId::new(0),
        on: ExistingNodeRef::new("slow"),
    });
    // The harness finishes `up` before `slow`, so `slow` is not final at splice
    // time: `f` must wait for the real edge to emit.
    let mut h = Harness::new(graph.clone())
        .results(BTreeMap::from([("up", splice_outcome(vec![request]))]));
    assert_eq!(h.run(), RunStatus::Success);
    let slow = h.started.iter().position(|n| n == "slow").expect("slow");
    let f = h.started.iter().position(|n| n == "f").expect("f");
    assert!(slow < f, "f waits for slow: {:?}", h.started);
    assert_eq!(h.status_of("f").as_deref(), Some("success"));
    h.verify_replay();
}

#[test]
fn a_final_reference_gates_without_waiting() {
    // `pre` finishes before the uploader (chained ahead of it), so the
    // reference is final at splice time and lowers to a guard, not an edge.
    let mut b = GraphBuilder::new();
    let pre = b.add_step("pre", ScopeId::new(0), NOOP);
    let up = b.add_step("up", ScopeId::new(0), NOOP);
    b.link(pre, up);
    b.node_mut(up).splice_policy = SplicePolicy::Append;
    let graph = b.build();

    let request = SpliceRequest::append(fragment(&["f"])).with_attachment(Attachment::DependsOn {
        node: NodeId::new(0),
        on: ExistingNodeRef::new("pre"),
    });
    let mut h = Harness::new(graph.clone())
        .results(BTreeMap::from([("up", splice_outcome(vec![request]))]));
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.status_of("f").as_deref(), Some("success"));
    h.verify_replay();
}

#[test]
fn a_non_all_dependent_rejects_the_request() {
    let mut b = GraphBuilder::new();
    let up = b.add_step("up", ScopeId::new(0), NOOP);
    let down = b.add_step("down", ScopeId::new(0), NOOP);
    b.link(up, down);
    b.set_join(down, JoinPolicy::Any);
    b.node_mut(up).splice_policy = SplicePolicy::Append;
    let graph = b.build();

    let mut h = Harness::new(graph.clone()).results(BTreeMap::from([(
        "up",
        splice_outcome(vec![SpliceRequest::append(fragment(&["a"]))]),
    )]));
    assert_eq!(h.run(), RunStatus::Failed);
    let info = h.state.history()[0]
        .outcome
        .status
        .failure_info()
        .expect("a failure");
    assert_eq!(info.class, INVALID_SPLICE_CLASS);
    assert!(info.message.contains("down"), "{}", info.message);
    h.verify_replay();
}

// ── Cancellation ──────────────────────────────────────────────────────────

#[test]
fn a_request_from_a_cancelled_scope_is_a_logged_no_op() {
    let graph = uploader_graph(SplicePolicy::Append);
    let before = graph.nodes.len();
    let mut h = Harness::new(graph.clone());
    h.feed(engine::Event::RunStarted);
    let starts = h.take_starts();
    let (firing, _) = starts.first().expect("up started");
    h.cancel(ir::CancelScopeId::ROOT);

    // The step returns with requests anyway; the whole list is dropped — no
    // policy check, no validation, no invalid_splice — and the outcome records
    // and routes under the normal cancel semantics.
    h.finish(
        *firing,
        splice_outcome(vec![SpliceRequest::append(fragment(&["a"]))]),
    );
    assert_eq!(h.state.graph.nodes.len(), before, "no fragment applied");
    assert!(h.state.errors().is_empty());
    let record = h
        .state
        .history()
        .iter()
        .find(|r| r.name == "up")
        .expect("up recorded");
    assert!(
        !record
            .outcome
            .status
            .failure_info()
            .is_some_and(|i| i.class == INVALID_SPLICE_CLASS),
        "a cancelled-scope request is dropped, never invalid_splice"
    );
    h.verify_replay();
}
