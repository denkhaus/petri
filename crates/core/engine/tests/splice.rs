//! Outcome-driven splice, the Append path: prepare-then-commit finalization,
//! policy, attachment, ordered composition, and atomic rejection. Named for the
//! rules they pin.

mod support;

use std::collections::BTreeMap;

use engine::INVALID_SPLICE_CLASS;
use ir::{
    Attachment, Edge, EdgeId, ExistingNodeRef, ExprTable, Graph, GraphBuilder, GraphFragment,
    JoinPolicy, Local, Node, NodeId, Outcome, ReplaceScope, RunStatus, Scope, ScopeId,
    SplicePolicy, SpliceRequest, StepRef, Value,
};
use support::{Harness, NOOP};

/// A linear fragment: `names` chained by unconditional edges, entry at the
/// first, exit at the last, one declared scope.
fn fragment(names: &[&str]) -> GraphFragment {
    GraphFragment::chain(
        names
            .iter()
            .map(|name| (*name, StepRef::new("noop", Value::Null))),
    )
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
        on:   ExistingNodeRef::new("x"),
    });
    let mut h = Harness::new(graph.clone()).results(BTreeMap::from([(
        "up",
        splice_outcome(vec![
            SpliceRequest::append(fragment(&["x"])),
            depends.clone(),
        ]),
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
        on:   ExistingNodeRef::new("slow"),
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
        on:   ExistingNodeRef::new("pre"),
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

// ── Replace and retraction ────────────────────────────────────────────────

/// A fragment whose interior join can park: `wa -> wj <- wb`, entries
/// `wa`/`wb`, exit `wj`. After `wa` finishes, `wj`'s admission is pending until
/// `wb` does.
fn parking_fragment(prefix: &str) -> GraphFragment {
    let name = |suffix: &str| format!("{prefix}{suffix}");
    let mut wa = Node::new(
        NodeId::<Local>::new(0),
        &name("a"),
        ScopeId::new(0),
        StepRef::new("noop", Value::Null),
    );
    let mut wb = Node::new(
        NodeId::new(1),
        &name("b"),
        ScopeId::new(0),
        StepRef::new("noop", Value::Null),
    );
    let wj = Node::new(
        NodeId::new(2),
        &name("j"),
        ScopeId::new(0),
        StepRef::new("noop", Value::Null),
    );
    wa.routing = ir::Routing::next(Edge::always(EdgeId::new(0), NodeId::new(2)));
    wb.routing = ir::Routing::next(Edge::always(EdgeId::new(1), NodeId::new(2)));
    GraphFragment {
        body:  ir::GraphBody {
            nodes:  vec![wa, wb, wj],
            scopes: vec![Scope::new(ScopeId::new(0))],
            exprs:  ExprTable::default(),
            entry:  vec![NodeId::new(0), NodeId::new(1)],
        },
        exits: vec![NodeId::new(2)],
    }
}

/// Three entries: uploader `up1` (Append), uploader `up2` with `policy`, and
/// `slow`, which the tests finish last.
fn two_uploader_graph(up2_policy: SplicePolicy) -> Graph {
    let mut b = GraphBuilder::new();
    let up1 = b.add_step("up1", ScopeId::new(0), NOOP);
    let up2 = b.add_step("up2", ScopeId::new(0), NOOP);
    let _slow = b.add_step("slow", ScopeId::new(0), NOOP);
    b.node_mut(up1).splice_policy = SplicePolicy::Append;
    b.node_mut(up2).splice_policy = up2_policy;
    b.build()
}

/// Drive: up1 appends a parking batch, wa finishes (wj parks, wb stays live),
/// then up2 replaces. Returns the harness after up2's outcome, with `wb` still
/// live, for the caller to finish the run its way.
fn parked_batch_then_replace(up2_policy: SplicePolicy, scope: ReplaceScope) -> Harness {
    let graph = two_uploader_graph(up2_policy);
    let mut h = Harness::new(graph);
    h.feed(engine::Event::RunStarted);
    let starts = h.take_starts();
    let by_name = |starts: &[(ir::FiringId, String)], name: &str| {
        starts
            .iter()
            .find(|(_, n)| n == name)
            .map(|(f, _)| *f)
            .expect("started")
    };
    let up1 = by_name(&starts, "up1");
    let up2 = by_name(&starts, "up2");
    let slow = by_name(&starts, "slow");

    h.finish(
        up1,
        splice_outcome(vec![SpliceRequest::append(parking_fragment("w"))]),
    );
    // Both entries start; `wj` parks once `wa` finishes, `wb` stays live.
    let batch_starts = h.take_starts();
    let wa = by_name(&batch_starts, "wa");
    let wb = by_name(&batch_starts, "wb");
    h.finish(wa, Outcome::success(Value::Null));

    h.finish(
        up2,
        splice_outcome(vec![SpliceRequest::replace(scope, GraphFragment::new())]),
    );
    // `wb` lived through the replace; `slow` and `wb` now finish.
    h.finish(slow, Outcome::success(Value::Null));
    h.finish(wb, Outcome::success(Value::Null));
    h
}

#[test]
fn own_batches_spares_another_uploaders_batch_and_live_firings_survive() {
    let mut h = parked_batch_then_replace(
        SplicePolicy::Replace {
            scope: ReplaceScope::OwnBatches,
        },
        ReplaceScope::OwnBatches,
    );
    // up2 owns no batch, so up1's parked `wj` survives, `wb` (live through the
    // replace) finishes normally, and `wj` fires once `wb`'s token arrives.
    let wj = h
        .take_starts()
        .iter()
        .find(|(_, n)| n == "wj")
        .map(|(f, _)| *f)
        .expect("wj fires: its admission was spared");
    h.finish(wj, Outcome::success(Value::Null));
    assert_eq!(h.status.expect("finished"), RunStatus::Success);
    assert_eq!(h.status_of("wb").as_deref(), Some("success"));
    h.verify_replay();
}

#[test]
fn all_pending_takes_another_uploaders_batch_and_the_run_still_quiesces() {
    let h = parked_batch_then_replace(
        SplicePolicy::Replace {
            scope: ReplaceScope::AllPending,
        },
        ReplaceScope::AllPending,
    );
    // `wj`'s parked admission was retracted: its token dropped, `wb`'s late
    // token swallowed by retraction identity, and the dead spliced region does
    // not block quiescence or terminal release.
    assert_eq!(
        h.start_count("wj"),
        0,
        "the retracted admission never fires"
    );
    assert!(h.status_of("wj").is_none(), "and leaves no record");
    assert_eq!(h.status.expect("finished"), RunStatus::Success);
    assert_eq!(h.state.held_scopes().count(), 0, "terminal release ran");
    let retraction = h
        .state
        .splices()
        .iter()
        .find_map(|batch| {
            let retracted: Vec<_> = batch
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    engine::SpliceEffect::Retract(admission) => Some(*admission),
                    engine::SpliceEffect::Supersede(_) => None,
                })
                .collect();
            (!retracted.is_empty()).then_some(retracted)
        })
        .expect("the batch records its retraction");
    assert_eq!(retraction.len(), 1);
    h.verify_replay();
}

#[test]
fn own_batches_retracts_the_same_uploaders_earlier_batch_across_generations() {
    // A loop-head uploader: fires at generation 0 and 1. NodeId is stable
    // across generations, so the second firing's OwnBatches reaches the first
    // firing's batch.
    let mut b = GraphBuilder::new();
    let up = b.add_step("up", ScopeId::new(0), NOOP);
    b.set_join(up, JoinPolicy::Any);
    b.set_budget(up, ir::Budget::looped(2));
    b.node_mut(up).splice_policy = SplicePolicy::Replace {
        scope: ReplaceScope::OwnBatches,
    };
    let again = {
        let generation = b.exprs().var("generation");
        let one = b.exprs().lit(1);
        b.exprs().binary(ir::BinOp::Lt, generation, one)
    };
    b.select(up, vec![ir::Arm::when(up, again).as_back()]);
    b.mark_entry(up);
    let graph = b.build();

    let mut h = Harness::new(graph);
    h.feed(engine::Event::RunStarted);
    let up_gen0 = h.take_starts()[0].0;
    h.finish(
        up_gen0,
        splice_outcome(vec![SpliceRequest::append(parking_fragment("w"))]),
    );
    let starts = h.take_starts();
    let wa = starts.iter().find(|(_, n)| n == "wa").unwrap().0;
    let up_gen1 = starts.iter().find(|(_, n)| n == "up").unwrap().0;
    h.finish(wa, Outcome::success(Value::Null));
    // `wj` parks. The same uploader's next generation retracts it.
    h.finish(
        up_gen1,
        splice_outcome(vec![SpliceRequest::replace(
            ReplaceScope::OwnBatches,
            GraphFragment::new(),
        )]),
    );
    let wb = h
        .state
        .live_firings()
        .find(|f| h.state.graph.node(f.node).is_some_and(|n| n.name == "wb"))
        .map(|f| f.id)
        .expect("wb is live");
    h.finish(wb, Outcome::success(Value::Null));

    assert_eq!(h.start_count("wj"), 0, "the uploader's own batch was taken");
    assert!(
        h.state.errors().is_empty(),
        "the gen-1 firing must not re-seed the gen-0 batch: {:?}",
        h.state.errors()
    );
    assert_eq!(h.status.expect("finished"), RunStatus::Success);
    h.verify_replay();
}

#[test]
fn a_retracted_admission_readmits_in_a_future_generation() {
    // `head -> b1 -> back`, with `b2` joining from generation 1: X (All over b1
    // and b2) parks at generation 0 forever, and completes at generation 1.
    // Retracting (X, 0) must not touch the generation-1 admission.
    let mut b = GraphBuilder::new();
    let head = b.add_step("head", ScopeId::new(0), NOOP);
    let b1 = b.add_step("b1", ScopeId::new(0), NOOP);
    let b2 = b.add_step("b2", ScopeId::new(0), NOOP);
    let x = b.add_step("x", ScopeId::new(0), NOOP);
    let up = b.add_step("up", ScopeId::new(0), NOOP);
    b.set_join(head, JoinPolicy::Any);
    b.set_budget(head, ir::Budget::looped(2));
    b.set_budget(b1, ir::Budget::looped(2));
    b.node_mut(up).splice_policy = SplicePolicy::Replace {
        scope: ReplaceScope::AllPending,
    };
    let second_lap = {
        let generation = b.exprs().var("generation");
        let one = b.exprs().lit(1);
        b.exprs().binary(ir::BinOp::Ge, generation, one)
    };
    let first_lap = {
        let generation = b.exprs().var("generation");
        let one = b.exprs().lit(1);
        b.exprs().binary(ir::BinOp::Lt, generation, one)
    };
    b.fan_out_groups(head, vec![vec![ir::Arm::always(b1)], vec![ir::Arm::when(
        b2, second_lap,
    )]]);
    b.fan_out_groups(b1, vec![vec![ir::Arm::always(x)], vec![
        ir::Arm::when(head, first_lap).as_back(),
    ]]);
    b.link(b2, x);
    b.graph_mut().entry = vec![head, up];
    let graph = b.build();

    let mut h = Harness::new(graph);
    h.feed(engine::Event::RunStarted);
    let starts = h.take_starts();
    let head_gen0 = starts.iter().find(|(_, n)| n == "head").unwrap().0;
    let up_firing = starts.iter().find(|(_, n)| n == "up").unwrap().0;
    h.finish(head_gen0, Outcome::success(Value::Null));
    let b1_gen0 = h.take_starts()[0].0;
    h.finish(b1_gen0, Outcome::success(Value::Null));
    // (x, 0) is parked — b2 never emits at generation 0 — and generation 1 is
    // under way. Retract everything pending.
    h.finish(
        up_firing,
        splice_outcome(vec![SpliceRequest::replace(
            ReplaceScope::AllPending,
            GraphFragment::new(),
        )]),
    );
    // Generation 1 runs to completion: b1, b2, then x at generation 1.
    loop {
        let starts = h.take_starts();
        if starts.is_empty() {
            break;
        }
        for (firing, _) in starts {
            h.finish(firing, Outcome::success(Value::Null));
        }
    }
    let record = h
        .state
        .history()
        .iter()
        .find(|r| r.name == "x")
        .expect("x completed in a later generation");
    assert_eq!(record.generation.raw(), 1);
    assert_eq!(h.status.expect("finished"), RunStatus::Success);
    h.verify_replay();
}

#[test]
fn a_reference_to_a_key_retracted_by_the_same_transaction_rejects() {
    // `x` joins All over `early` and `slow`; `early`'s token parks it. One
    // outcome then retracts (x, 0) and tries to depend on `x`.
    let mut b = GraphBuilder::new();
    let early = b.add_step("early", ScopeId::new(0), NOOP);
    let slow = b.add_step("slow", ScopeId::new(0), NOOP);
    let x = b.add_step("x", ScopeId::new(0), NOOP);
    let up = b.add_step("up", ScopeId::new(0), NOOP);
    b.link(early, x);
    b.link(slow, x);
    b.node_mut(up).splice_policy = SplicePolicy::Replace {
        scope: ReplaceScope::AllPending,
    };
    b.graph_mut().entry = vec![early, slow, up];
    let graph = b.build();
    let before = graph.nodes.len();

    let mut h = Harness::new(graph);
    h.feed(engine::Event::RunStarted);
    let starts = h.take_starts();
    let early_f = starts.iter().find(|(_, n)| n == "early").unwrap().0;
    let up_f = starts.iter().find(|(_, n)| n == "up").unwrap().0;
    let slow_f = starts.iter().find(|(_, n)| n == "slow").unwrap().0;
    h.finish(early_f, Outcome::success(Value::Null));

    let depends = SpliceRequest::append(fragment(&["f"])).with_attachment(Attachment::DependsOn {
        node: NodeId::new(0),
        on:   ExistingNodeRef::new("x"),
    });
    h.finish(
        up_f,
        splice_outcome(vec![
            SpliceRequest::replace(ReplaceScope::AllPending, GraphFragment::new()),
            depends,
        ]),
    );
    // Atomic: no fragment, no retraction, and the canonical class.
    assert_eq!(h.state.graph.nodes.len(), before);
    let info = h
        .state
        .history()
        .iter()
        .find(|r| r.name == "up")
        .and_then(|r| r.outcome.status.failure_info().cloned())
        .expect("up failed");
    assert_eq!(info.class, INVALID_SPLICE_CLASS);
    h.finish(slow_f, Outcome::success(Value::Null));
    let x_f = h.take_starts().first().map(|(f, _)| *f).expect("x fires");
    h.finish(x_f, Outcome::success(Value::Null));
    assert_eq!(
        h.status_of("x").as_deref(),
        Some("success"),
        "the rejected transaction retracted nothing"
    );
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
        record
            .outcome
            .status
            .failure_info()
            .is_none_or(|i| i.class != INVALID_SPLICE_CLASS),
        "a cancelled-scope request is dropped, never invalid_splice"
    );
    h.verify_replay();
}
