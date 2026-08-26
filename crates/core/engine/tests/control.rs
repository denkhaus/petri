//! §6: `ControlRequested` — the host delivers a value into a live firing. Only
//! `Control::Deliver` to a live, not-cancelling, not-awaiting-retry firing produces
//! a command; everything else is a logged no-op, never a `RunError`, because a late
//! answer must not fail the run.

mod support;

use engine::{Command, Event};
use ir::{Control, FiringId, GraphBuilder, Outcome, RetryPolicy, RunStatus, Value, validate};
use serde_json::json;
use support::{Harness, NOOP};

fn chain() -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let work = b.add_step("work", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(work, after);
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

fn deliveries(h: &Harness) -> Vec<(FiringId, Control)> {
    h.commands
        .iter()
        .filter_map(|c| match c {
            Command::DeliverControl { firing, ctl } => Some((*firing, ctl.clone())),
            _ => None,
        })
        .collect()
}

/// A deliver to a live firing produces exactly one `DeliverControl`, with the
/// payload intact. No state change, no routing effect.
#[test]
fn a_deliver_to_a_live_firing_produces_one_command() {
    let mut h = Harness::new(chain());
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    let (firing, _) = starts[0];
    h.commands.clear();

    let before = h.state.pending_count();
    h.feed(Event::ControlRequested {
        firing,
        ctl: Control::Deliver(json!({"answer": "approved"})),
    });
    assert_eq!(
        deliveries(&h),
        vec![(firing, Control::Deliver(json!({"answer": "approved"})))]
    );
    assert_eq!(h.state.pending_count(), before, "no routing effect");
    assert!(h.state.errors().is_empty());

    h.finish(firing, Outcome::success(Value::Null));
}

/// Steering is a stream: repeated delivers to one firing produce repeated
/// commands, in order.
#[test]
fn repeated_delivers_are_a_stream() {
    let mut h = Harness::new(chain());
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    let (firing, _) = starts[0];
    h.commands.clear();

    for n in 0..3 {
        h.feed(Event::ControlRequested {
            firing,
            ctl: Control::Deliver(json!(n)),
        });
    }
    let delivered = deliveries(&h);
    assert_eq!(delivered.len(), 3);
    for (n, (to, ctl)) in delivered.into_iter().enumerate() {
        assert_eq!(to, firing);
        assert_eq!(ctl, Control::Deliver(json!(n)));
    }
}

/// A deliver to a finished or unknown firing is a logged no-op: no command, no
/// error. The event is in the log either way — the audit trail.
#[test]
fn a_deliver_to_a_dead_or_unknown_firing_is_a_logged_noop() {
    let mut h = Harness::new(chain());
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    let (firing, _) = starts[0];
    h.finish(firing, Outcome::success(Value::Null));
    let afters = h.take_starts();
    h.commands.clear();

    // Finished.
    h.feed(Event::ControlRequested {
        firing,
        ctl: Control::Deliver(json!("late answer")),
    });
    // Unknown.
    h.feed(Event::ControlRequested {
        firing: FiringId::new(999),
        ctl: Control::Deliver(json!("to nobody")),
    });

    assert!(deliveries(&h).is_empty());
    assert!(h.state.errors().is_empty(), "{:?}", h.state.errors());
    assert_eq!(
        h.state
            .log
            .events()
            .filter(|e| matches!(e, Event::ControlRequested { .. }))
            .count(),
        2,
        "both requests are in the log"
    );

    // The run still finishes clean.
    h.finish(afters[0].0, Outcome::success(Value::Null));
    assert_eq!(h.status, Some(RunStatus::Success));
}

/// A firing waiting out a retry backoff has no task to deliver to: no command, no
/// error.
#[test]
fn a_deliver_to_an_awaiting_retry_firing_is_a_noop() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let work = b.add_step("work", scope, NOOP);
    b.node_mut(work).retry = RetryPolicy::attempts(2);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    let (firing, _) = starts[0];
    h.finish(firing, Outcome::failure("first try"));
    assert!(
        h.state.firing(firing).is_some_and(|f| f.awaiting_retry),
        "the firing waits out its backoff"
    );

    h.feed(Event::ControlRequested {
        firing,
        ctl: Control::Deliver(json!("answer")),
    });
    assert!(deliveries(&h).is_empty());
    assert!(h.state.errors().is_empty());

    h.drain_retries();
    let retries = h.take_starts();
    h.finish(firing, Outcome::success(Value::Null));
    assert_eq!(retries.len(), 1);
}

/// `ControlRequested` carrying `Cancel` or `Kill` is a logged no-op: both have
/// their own scope-routed events whose closure bookkeeping (`cancelling`, kill
/// tiers, `run_on_cancel` admission) a raw per-firing path would bypass.
#[test]
fn cancel_and_kill_do_not_ride_the_per_firing_path() {
    let mut h = Harness::new(chain());
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    let (firing, _) = starts[0];
    h.commands.clear();

    for ctl in [Control::Cancel, Control::Kill] {
        h.feed(Event::ControlRequested { firing, ctl });
    }
    assert!(deliveries(&h).is_empty(), "no control was delivered");
    assert!(h.state.errors().is_empty());
    assert!(
        h.state.firing(firing).is_some_and(|f| !f.cancelling),
        "no cancellation bookkeeping happened"
    );
    assert!(!h.state.is_cancelled());

    // The firing finishes and routes like any healthy one.
    h.finish(firing, Outcome::success(Value::Null));
    let afters = h.take_starts();
    assert_eq!(afters.len(), 1, "routing was untouched");
    h.finish(afters[0].0, Outcome::success(Value::Null));
    assert_eq!(h.status, Some(RunStatus::Success));
}

/// The determinism canary: a log holding `ControlRequested` records — commands
/// produced, no-ops included — replays byte-identically.
#[test]
fn replay_is_byte_identical_with_control_requests_in_the_log() {
    let mut h = Harness::new(chain());
    h.feed(Event::RunStarted);
    let starts = h.take_starts();
    let (firing, _) = starts[0];
    h.feed(Event::ControlRequested {
        firing,
        ctl: Control::Deliver(json!({"answer": 42})),
    });
    h.finish(firing, Outcome::success(json!("done")));
    // A late one, after the firing finished: the logged no-op shape.
    h.feed(Event::ControlRequested {
        firing,
        ctl: Control::Deliver(json!("too late")),
    });
    let afters = h.take_starts();
    h.finish(afters[0].0, Outcome::success(Value::Null));
    assert_eq!(h.status, Some(RunStatus::Success));

    h.verify_replay();
}
