//! Typed capabilities on `StepCtx`: a host-registered service reaches the step
//! by type, and a step missing its service fails its node routably — never the
//! run machinery.

mod support;

use std::sync::Arc;

use driver::RunConfig;
use executor::MapSecrets;
use ir::{Arm, BinOp, GraphBuilder, RunStatus, ScopeId};
use serde_json::json;
use steps::{CAPABILITY_UNAVAILABLE_CLASS, Capabilities};
use support::*;

fn greet_registry() -> steps::Registry {
    let mut registry = runners();
    registry.register_runner(Arc::new(GreetStep));
    registry
}

/// With the capability registered on the driver, the step consumes it.
#[tokio::test]
async fn a_registered_capability_reaches_the_step() {
    let dir = RunDir::new("caps-present");
    let mut b = GraphBuilder::new();
    b.add_step("greet", ScopeId::new(0), GREET_KIND);
    let report = host_driver_full(
        b.build(),
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()),
        greet_registry(),
    )
    .with_capabilities(Capabilities::builder().provide(Greeting("hello")).build())
    .await_run()
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "greet"), json!("hello"));
}

/// Without it, the node fails with `capability_unavailable` — and the failure
/// routes like any other: a guarded rescue arm still fires.
#[tokio::test]
async fn a_missing_capability_fails_the_node_routably() {
    let dir = RunDir::new("caps-absent");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let greet = b.add_step("greet", scope, GREET_KIND);
    let rescue = add_script(&mut b, "rescue", scope, "echo rescued");
    let failed = {
        let e = b.exprs();
        let status = e.var("status");
        let failure = e.lit("failure");
        e.binary(BinOp::Eq, status, failure)
    };
    b.select(greet, vec![Arm::when(rescue, failed)]);
    let graph = b.build();

    let report = host_driver_full(
        graph.clone(),
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()),
        greet_registry(),
    )
    .await_run()
    .await;

    assert_eq!(report.status, RunStatus::Failed, "the failure is recorded");
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "greet")
        .expect("recorded");
    let failure = record.outcome.status.failure_info().expect("a failure");
    assert_eq!(failure.class.as_str(), CAPABILITY_UNAVAILABLE_CLASS);
    assert!(
        failure.message.contains("Greeting"),
        "the message names the missing type: {}",
        failure.message
    );
    assert_eq!(
        status_of(&report, "rescue").as_deref(),
        Some("success"),
        "the failure routed; the run machinery is untouched"
    );
    assert_replay_identical(&graph, &report);
}
