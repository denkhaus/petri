//! `Runtime::capability` end to end: a host service registered on the builder
//! reaches a step through `StepCtx`, and a run without it fails the node — not
//! the run machinery.

use std::sync::Arc;

use petri::ir::{Graph, GraphBuilder, RunStatus, ScopeId};
use petri::{RunOptions, steps};
use serde_json::json;
use testkit::{GREET_KIND, GreetStep, Greeting, RunDir};

fn greet_graph() -> Graph {
    let mut b = GraphBuilder::new();
    b.add_step("greet", ScopeId::new(0), GREET_KIND);
    b.build()
}

fn greet_registry() -> steps::Registry {
    let mut registry = steps::Registry::new();
    registry.register_runner(Arc::new(GreetStep));
    registry
}

#[tokio::test]
async fn a_capability_registered_on_the_runtime_reaches_the_step() {
    let dir = RunDir::new("rt-caps");
    let rt = petri::runtime()
        .steps(greet_registry())
        .capability(Greeting("from the runtime"))
        .options(RunOptions::new(dir.path()));
    let report = rt.run(greet_graph()).await.expect("replays");
    assert_eq!(report.status, RunStatus::Success);
    assert_eq!(
        report
            .state
            .history()
            .iter()
            .find(|r| r.name == "greet")
            .expect("recorded")
            .outcome
            .output,
        json!("from the runtime")
    );
}

#[tokio::test]
async fn without_the_capability_the_node_fails_and_the_run_reports() {
    let dir = RunDir::new("rt-caps-absent");
    let rt = petri::runtime()
        .steps(greet_registry())
        .options(RunOptions::new(dir.path()));
    let report = rt.run(greet_graph()).await.expect("replays");
    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "greet")
        .expect("the node failed, the machinery did not");
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some(steps::CAPABILITY_UNAVAILABLE_CLASS)
    );
}
