//! Handoff §7 test 6: `Budget.timeout` runs the same ladder, and the race with a
//! natural exit is resolved by arrival order, which the log makes canonical.

mod support;

use std::time::Duration;

use driver::RunConfig;
use executor::{MapSecrets, Retention};
use ir::{Budget, GraphBuilder, RunStatus, ScopeId, StepRef, validate};
use serde_json::json;
use steps::PROCESS_KIND;
use support::*;

fn timed_step(run: &str, limit: Duration) -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let node = b.add_node("slow", scope, StepRef::new(PROCESS_KIND, script(run)));
    b.set_budget(node, Budget::new(1, limit));
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

/// A step that outlives its budget is `TimedOut`, not `Cancelled`: only the driver
/// knows a timer got there first.
#[tokio::test]
async fn a_step_that_outlives_its_budget_times_out() {
    let dir = RunDir::new("timeout");
    let graph = timed_step("echo starting; sleep 30", Duration::from_millis(300));
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_millis(500))
        .with_retention(Retention::Never);

    let report = host_driver_with(graph.clone(), &dir, MapSecrets::empty(), config)
        .await_run()
        .await;

    assert_eq!(report.status, RunStatus::Failed, "a timeout fails the run");
    assert_eq!(status_of(&report, "slow").as_deref(), Some("timed_out"));
    // It went through the same ladder as a cancel.
    let escalation = output_of(&report, "slow")["cancel_escalation"].clone();
    assert!(
        escalation == json!("sigterm") || escalation == json!("sigkill"),
        "the timeout used the cancellation ladder: {escalation}"
    );
    assert_replay_identical(&graph, &report);
}

/// The timeout is per attempt, not per firing: each retry gets the whole budget.
#[tokio::test]
async fn the_timeout_applies_per_attempt() {
    let dir = RunDir::new("timeout-per-attempt");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let node = b.add_node(
        "flaky",
        scope,
        StepRef::new(
            PROCESS_KIND,
            script(r#"if [ -f attempted ]; then echo second; else touch attempted; sleep 30; fi"#),
        ),
    );
    b.set_budget(node, Budget::new(1, Duration::from_millis(400)));
    b.node_mut(node).retry = ir::RetryPolicy::attempts(2)
        .with_retry_on(ir::RetryOn::statuses(vec![ir::StatusKind::TimedOut]))
        .with_backoff(ir::Backoff {
            initial: Duration::from_millis(10),
            factor: 1.0,
            max: Duration::from_millis(50),
            jitter: true,
        });
    let graph = b.build();
    validate(&graph).expect("valid");

    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_millis(400))
        .with_retention(Retention::Always);
    let report = host_driver_with(graph.clone(), &dir, MapSecrets::empty(), config)
        .await_run()
        .await;

    // The first attempt timed out; the second got a fresh budget and finished.
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "flaky")
        .expect("recorded");
    assert_eq!(record.attempt.raw(), 2, "the retry got its own budget");
    assert_replay_identical(&graph, &report);
}

/// A timeout racing a natural exit resolves one way or the other in real time, and
/// replay reproduces whichever it was — the log made the arrival order canonical.
#[tokio::test]
async fn a_timeout_racing_a_natural_exit_replays_identically() {
    for attempt in 0..8 {
        let dir = RunDir::new(&format!("timeout-race-{attempt}"));
        // The script and the budget are deliberately the same length, so which
        // terminal arrives first is a wall-clock accident.
        let graph = timed_step("sleep 0.25", Duration::from_millis(250));
        let config = RunConfig::new(dir.path())
            .with_grace(Duration::from_millis(500))
            .with_retention(Retention::Never);

        let report = host_driver_with(graph.clone(), &dir, MapSecrets::empty(), config)
            .await_run()
            .await;

        let status = status_of(&report, "slow").expect("the step finished either way");
        assert!(
            status == "success" || status == "timed_out",
            "the race resolves to exactly one of the two, got {status}"
        );
        // Whichever way it went, the log fixed it, and replay agrees.
        assert_replay_identical(&graph, &report);
    }
}
