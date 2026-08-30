//! Handoff §4: `PartialSuccess` is first-class, and `Status::is_success_like`
//! is the single place success-likeness is defined.

mod support;

use ir::{
    Arm, GraphBuilder, JoinPolicy, Outcome, RetryPolicy, RunStatus, Status, StatusKind, StepRef,
    Value, validate,
};
use serde_json::json;
use support::{Harness, NOOP, process_outcome};

/// Handoff §7 test 5. A soft-failed process step: exit 1 under `soft_fail:
/// [1]`. Joins and default success guards treat it as a success, a
/// `partial_success` guard can still route it distinctly, the log keeps the
/// underlying exit status, and retry never triggers.
#[test]
fn a_soft_failed_step_is_success_like_but_still_distinguishable() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let build = b.add_step("build", scope, NOOP);
    let deploy = b.add_step("deploy", scope, NOOP);
    let warn = b.add_step("warn", scope, NOOP);
    let finish = b.add_step("finish", scope, NOOP);

    b.node_mut(build).step = StepRef::new(NOOP, json!({ "soft_fail": [1] }));
    // A retry policy that would fire on any failure. It must not fire here.
    b.node_mut(build).retry = RetryPolicy::attempts(4);

    let (succeeded, was_partial) = {
        let e = b.exprs();
        (e.call("success", vec![]), e.call("partial_success", vec![]))
    };
    // Two groups: the default success path and a distinct soft-failure path. Both
    // fire, which is what "success-like, but still visible" means.
    b.fan_out_groups(build, vec![vec![Arm::when(deploy, succeeded)], vec![
        Arm::when(warn, was_partial),
    ]]);
    b.link(deploy, finish);
    b.link(warn, finish);
    b.set_join(finish, JoinPolicy::All);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "build" => process_outcome(&info.config, 1),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(
        h.run(),
        RunStatus::Success,
        "a soft failure does not fail the run"
    );

    assert_eq!(h.start_count("build"), 1, "retry never triggers");
    assert_eq!(
        h.start_count("deploy"),
        1,
        "the default success guard passes"
    );
    assert_eq!(
        h.start_count("warn"),
        1,
        "and the soft path is still visible"
    );
    assert_eq!(h.start_count("finish"), 1, "the All join is satisfied");

    // The log keeps the real failure.
    let record = h
        .state
        .history()
        .iter()
        .find(|r| r.name == "build")
        .unwrap();
    assert_eq!(record.outcome.status.tag(), "partial_success");
    let underlying = record.outcome.status.failure_info().expect("kept");
    assert_eq!(underlying.class, "exit_status:1");
    assert_eq!(record.outcome.output, json!({ "exit_code": 1 }));
    h.verify_replay();
}

/// Without `soft_fail`, the same exit status is an ordinary failure.
#[test]
fn without_soft_fail_the_same_exit_status_fails() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let build = b.add_step("build", scope, NOOP);
    let deploy = b.add_step("deploy", scope, NOOP);
    let succeeded = b.exprs().call("success", vec![]);
    b.select(build, vec![Arm::when(deploy, succeeded)]);
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|info| process_outcome(&info.config, 1));
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.start_count("deploy"), 0);
}

/// `soft_fail` lists exit statuses: one that is not listed still fails.
#[test]
fn soft_fail_only_covers_the_listed_exit_statuses() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let build = b.add_step("build", scope, NOOP);
    b.node_mut(build).step = StepRef::new(NOOP, json!({ "soft_fail": [1] }));
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|info| process_outcome(&info.config, 2));
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(
        h.status_of("build").as_deref(),
        Some("failure"),
        "exit 2 is not on the list"
    );
}

/// The classification point. Nothing else may open-code this match.
#[test]
fn is_success_like_is_the_only_classification() {
    assert!(Status::Success.is_success_like());
    assert!(Status::partial_clean().is_success_like());
    assert!(Status::partial(ir::FailureInfo::exit_status(1)).is_success_like());
    assert!(!Status::failure("no").is_success_like());
    assert!(!Status::Skipped.is_success_like());
    assert!(!Status::Cancelled.is_success_like());
    assert!(!Status::TimedOut.is_success_like());

    // Success-likeness and failure are separate questions: a partial success is
    // neither a failure nor a clean success.
    assert!(!Status::partial(ir::FailureInfo::exit_status(1)).is_failure());
    assert!(Status::failure("no").is_failure());
    assert!(Status::TimedOut.is_failure());
}

/// A `PartialSuccess` converted from a failure always carries it; a clean
/// partial completion carries nothing, because nothing failed.
#[test]
fn a_converted_partial_success_keeps_the_failure() {
    let converted = Status::partial(ir::FailureInfo::exit_status(3));
    assert_eq!(
        converted.failure_info().map(|f| f.class.as_str()),
        Some("exit_status:3")
    );

    let clean = Status::partial_clean();
    assert_eq!(clean.failure_info(), None);
    assert_eq!(clean.tag(), "partial_success");
}

/// Every status has a kind, so `retry_on` can match on the variant without the
/// payload.
#[test]
fn status_kinds_cover_the_closed_enum() {
    let all = [
        (Status::Success, StatusKind::Success),
        (Status::partial_clean(), StatusKind::PartialSuccess),
        (Status::failure("x"), StatusKind::Failure),
        (Status::Skipped, StatusKind::Skipped),
        (Status::Cancelled, StatusKind::Cancelled),
        (Status::TimedOut, StatusKind::TimedOut),
    ];
    for (status, kind) in all {
        assert_eq!(status.kind(), kind, "{status:?}");
    }
}

/// A soft failure satisfies a downstream `All` join like any success would.
#[test]
fn a_soft_failure_satisfies_a_downstream_join() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let soft = b.add_step("soft", scope, NOOP);
    let clean = b.add_step("clean", scope, NOOP);
    let join = b.add_step("join", scope, NOOP);
    b.node_mut(soft).step = StepRef::new(NOOP, json!({ "soft_fail": true }));
    b.fan_out(start, &[soft, clean]);
    b.link(soft, join);
    b.link(clean, join);
    b.set_join(join, JoinPolicy::All);
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "soft" => process_outcome(&info.config, 9),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("join"), 1);
    assert_eq!(h.status_of("soft").as_deref(), Some("partial_success"));
}
