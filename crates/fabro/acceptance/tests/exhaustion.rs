//! Readiness item 4, failure policy after retries: the last retryable failure
//! is finalized the way the pinned Fabro finalizes it. Fabro's executor
//! (`fabro-core/src/executor.rs`, `execute_with_retry` then
//! `apply_succeed_policy`) turns the exhausted retry into an ordinary failure
//! (`finalize_retries_exhausted`; `allow_partial` makes it a partial success
//! instead), then checks the node's explicit routes against that failure and
//! promotes it under `on_failure="succeed"` only when none matches, and
//! routes the effective result.
//!
//! These cases run under stubs like the oracle battery in `routing.rs`, but
//! they have no committed Fabro fixture: the pinned Fabro binary was not
//! available when they were written (`scripts/oracle-regenerate.sh` builds it
//! from the fetched corpus), so the expectations below are derived from the
//! pinned source named above rather than captured from the binary. Review
//! finding G05 is the reproduction they close.

use std::collections::BTreeMap;

use fabro_acceptance::runs::{Case, RunResult};
use serde_json::{Value, json};

/// A scripted case whose retryable stage `a` fails with Fabro's retry outcome
/// on every attempt.
fn exhausted(name: &str, body: &str, script: Value) -> Case {
    let workflow =
        format!("digraph T {{\n  start [shape=Mdiamond]\n  exit [shape=Msquare]\n{body}\n}}\n");
    let mut scripts = BTreeMap::new();
    scripts.insert("a".to_string(), script);
    Case {
        name: name.to_string(),
        workflow,
        scripts,
        departure: None,
    }
}

fn retry_requested() -> Value {
    json!({
        "outcome": "failed",
        "failure_class": "retry_requested",
        "failure_reason": "flaky"
    })
}

fn assert_path(result: &RunResult, status: &str, path: &[&str]) {
    assert_eq!(result.status, status, "{result:?}");
    assert_eq!(result.nodes(), path, "{result:?}");
}

/// The probe of review finding G05, under stubs: `on_failure="succeed"` with
/// an explicit `outcome=failed` edge. After the retries the failure takes
/// that edge; it is not promoted past it.
#[tokio::test]
async fn an_exhausted_retry_under_succeed_takes_an_explicit_failure_edge() {
    let case = exhausted(
        "exhausted_succeed_explicit_edge",
        r#"
  a [prompt="x", max_retries=1, on_failure="succeed"]
  recover [prompt="x"]
  b [prompt="x"]
  start -> a
  a -> recover [condition="outcome=failed"]
  a -> b
  recover -> exit
  b -> exit"#,
        retry_requested(),
    );
    let r = case.run().await;
    assert_path(&r, "success", &["start", "a", "recover", "exit"]);
    assert_eq!(r.path[1].outcome, "failed");
}

/// A preferred label on the exhausted failure names a labelled edge: that
/// edge is explicit, so the failure stays failed and takes it.
#[tokio::test]
async fn an_exhausted_retry_under_succeed_takes_a_preferred_label() {
    let case = exhausted(
        "exhausted_succeed_preferred_label",
        r#"
  a [prompt="x", max_retries=1, on_failure="succeed"]
  fix [prompt="x"]
  b [prompt="x"]
  start -> a
  a -> fix [label="[F] Fix"]
  a -> b
  fix -> exit
  b -> exit"#,
        json!({
            "outcome": "failed",
            "failure_class": "retry_requested",
            "failure_reason": "flaky",
            "preferred_label": "[F] Fix"
        }),
    );
    let r = case.run().await;
    assert_path(&r, "success", &["start", "a", "fix", "exit"]);
    assert_eq!(r.path[1].outcome, "failed");
}

/// A suggested target on the exhausted failure names an edge: explicit too.
#[tokio::test]
async fn an_exhausted_retry_under_succeed_takes_a_suggested_target() {
    let case = exhausted(
        "exhausted_succeed_suggested_target",
        r#"
  a [prompt="x", max_retries=1, on_failure="succeed"]
  b [prompt="x"]
  c [prompt="x"]
  start -> a
  a -> b
  a -> c
  b -> exit
  c -> exit"#,
        json!({
            "outcome": "failed",
            "failure_class": "retry_requested",
            "failure_reason": "flaky",
            "suggested_next_ids": ["c"]
        }),
    );
    let r = case.run().await;
    assert_path(&r, "success", &["start", "a", "c", "exit"]);
    assert_eq!(r.path[1].outcome, "failed");
}

/// No explicit route matches the exhausted failure: it is promoted, reports
/// `succeeded` as Fabro reports it, and an `outcome=succeeded` edge then
/// matches the effective result.
#[tokio::test]
async fn an_exhausted_retry_under_succeed_is_promoted_and_routes_as_succeeded() {
    let case = exhausted(
        "exhausted_succeed_promoted",
        r#"
  a [prompt="x", max_retries=1, on_failure="succeed"]
  b [prompt="x"]
  start -> a
  a -> b [condition="outcome=succeeded"]
  a -> exit
  b -> exit"#,
        retry_requested(),
    );
    let r = case.run().await;
    assert_path(&r, "success", &["start", "a", "b", "exit"]);
    assert_eq!(r.path[1].outcome, "succeeded");
}

/// `on_retries_exhausted="succeed"` on its own, with `on_failure="route"`:
/// the exhausted retry is promoted; an ordinary failure of the same node is
/// not.
#[tokio::test]
async fn the_exhaustion_policy_applies_only_to_the_exhausted_retry() {
    let body = r#"
  a [prompt="x", max_retries=1, on_failure="route", on_retries_exhausted="succeed"]
  b [prompt="x"]
  start -> a
  a -> b [condition="outcome=succeeded"]
  a -> exit
  b -> exit"#;
    let r = exhausted("exhaustion_policy_exhausted", body, retry_requested())
        .run()
        .await;
    assert_path(&r, "success", &["start", "a", "b", "exit"]);
    assert_eq!(r.path[1].outcome, "succeeded");
    let r = exhausted(
        "exhaustion_policy_ordinary_failure",
        body,
        json!({ "outcome": "failed", "failure_reason": "boom" }),
    )
    .run()
    .await;
    assert_path(&r, "success", &["start", "a", "exit"]);
    assert_eq!(r.path[1].outcome, "failed");
}

/// `allow_partial=true`: Fabro finalizes the exhausted retry as a partial
/// success before any route is considered, so an explicit `outcome=failed`
/// edge does not catch it and the unconditional edge is taken.
#[tokio::test]
async fn an_exhausted_retry_under_allow_partial_is_accepted_before_any_route() {
    let case = exhausted(
        "exhausted_allow_partial_explicit_edge",
        r#"
  a [prompt="x", max_retries=1, allow_partial=true]
  recover [prompt="x"]
  b [prompt="x"]
  start -> a
  a -> recover [condition="outcome=failed"]
  a -> b
  recover -> exit
  b -> exit"#,
        retry_requested(),
    );
    let r = case.run().await;
    assert_path(&r, "success", &["start", "a", "b", "exit"]);
    assert_eq!(r.path[1].outcome, "partially_succeeded");
}

/// A retry that succeeds before the attempts run out is a plain success: the
/// exhaustion policy never sees it.
#[tokio::test]
async fn a_retry_that_succeeds_in_time_is_not_finalized() {
    let case = exhausted(
        "retry_then_success_under_succeed",
        r#"
  a [prompt="x", max_retries=1, on_failure="succeed"]
  recover [prompt="x"]
  b [prompt="x"]
  start -> a
  a -> recover [condition="outcome=failed"]
  a -> b
  recover -> exit
  b -> exit"#,
        json!({
            "calls": [
                { "outcome": "failed", "failure_class": "retry_requested" },
                { "outcome": "succeeded" }
            ]
        }),
    );
    let r = case.run().await;
    assert_path(&r, "success", &["start", "a", "b", "exit"]);
    assert_eq!(r.path[1].outcome, "succeeded");
}

/// The probe's own shape: a human gate whose question expires with no
/// default fails with the retry outcome. Under `on_failure="succeed"` the
/// explicit `outcome=failed` edge is checked first and taken; without one
/// the gate is promoted and, as a success, takes its unconditional edge.
#[tokio::test]
async fn an_expired_human_gate_under_succeed_takes_its_explicit_failure_edge_first() {
    let gate = |name: &str, body: &str| {
        let mut case = exhausted(name, body, Value::Null);
        case.scripts.clear();
        case.scripts.insert("gate".to_string(), retry_requested());
        case
    };
    let r = gate(
        "expired_gate_explicit_edge",
        r#"
  gate [shape=hexagon, label="Continue?", max_retries=0, on_failure="succeed"]
  recover [prompt="x"]
  fallthrough [prompt="x"]
  start -> gate
  gate -> recover [condition="outcome=failed", label="[R] Recover"]
  gate -> fallthrough [label="[Y] Continue"]
  recover -> exit
  fallthrough -> exit"#,
    )
    .run()
    .await;
    assert_path(&r, "success", &["start", "gate", "recover", "exit"]);
    assert_eq!(r.path[1].outcome, "failed");
    let r = gate(
        "expired_gate_no_explicit_edge",
        r#"
  gate [shape=hexagon, label="Continue?", max_retries=0, on_failure="succeed"]
  fallthrough [prompt="x"]
  start -> gate
  gate -> fallthrough [label="[Y] Continue"]
  fallthrough -> exit"#,
    )
    .run()
    .await;
    assert_path(&r, "success", &["start", "gate", "fallthrough", "exit"]);
    assert_eq!(r.path[1].outcome, "succeeded");
}
