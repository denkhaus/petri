//! Handoff §7 tests 7 and 10: acquire failure and workspace retention.

mod support;

use std::time::Duration;

use driver::RunConfig;
use executor::{MapSecrets, Retention};
use ir::{GraphBuilder, RunStatus, ScopeId, StepRef, validate};
use serde_json::json;
use steps::PROCESS_KIND;
use support::*;

/// §7 test 7. An environment that cannot be acquired fails every firing in its
/// scope with `env_acquire` — routable like any other failure, never a run
/// abort. A cleanup path in a working scope still runs on `if: failure()`, and
/// the run completes.
#[tokio::test]
async fn an_acquire_failure_fails_its_firings_and_still_routes() {
    let dir = RunDir::new("acquire-failure");

    let mut b = GraphBuilder::bare();
    let broken = b.add_scope(ir::Scope::new(ScopeId::new(0)));
    let healthy = b.add_scope(ir::Scope::new(ScopeId::new(0)));
    let build = b.add_node(
        "build",
        broken,
        StepRef::new(PROCESS_KIND, script("echo never runs")),
    );
    let sibling = b.add_node(
        "sibling",
        broken,
        StepRef::new(PROCESS_KIND, script("echo also never runs")),
    );
    // Cleanup lives in a scope that works: a broken environment is exactly when you
    // cannot rely on it to clean up after itself.
    let cleanup = b.add_node(
        "cleanup",
        healthy,
        StepRef::new(PROCESS_KIND, script("echo cleaning up")),
    );

    let failed = b.exprs().call("failure", vec![]);
    b.fan_out(build, &[sibling, cleanup]);
    b.set_precondition(cleanup, failed);
    let graph = b.build();
    validate(&graph).expect("valid");

    let report = broken_scope_driver(graph, &dir, &[broken], "no such image: not-a-real-image:v0")
        .await_run()
        .await;

    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "build")
        .expect("build has an outcome");
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some("env_acquire"),
        "the failure names the acquire"
    );
    assert!(
        record
            .outcome
            .status
            .failure_info()
            .is_some_and(|f| f.message.contains("not-a-real-image")),
        "and carries the daemon's message"
    );

    assert_eq!(
        report
            .state
            .history()
            .iter()
            .filter(
                |r| r.outcome.status.failure_info().map(|f| f.class.as_str())
                    == Some("env_acquire")
            )
            .count(),
        2,
        "both firings in the broken scope failed the same way"
    );
    assert_eq!(
        status_of(&report, "cleanup").as_deref(),
        Some("success"),
        "the failure() cleanup path routed and ran"
    );
    assert!(report.state.is_finished(), "the run completed");
}

/// §7 test 10, host half. A failed scope keeps its workspace — that is what you
/// need to debug it — and a successful one does not.
#[tokio::test]
async fn workspace_retention_follows_the_outcome() {
    // Failure keeps it.
    let dir = RunDir::new("retain-on-failure");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_node(
        "boom",
        scope,
        StepRef::new(PROCESS_KIND, script("echo evidence > evidence.txt; exit 3")),
    );
    let graph = b.build();
    let workspace = dir.workspace();
    let report = host_driver_with(
        graph,
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()).with_retention(Retention::OnFailure),
    )
    .await_run()
    .await;
    assert_eq!(report.status, RunStatus::Failed);
    assert!(
        workspace.join("evidence.txt").exists(),
        "a failed scope's workspace is kept"
    );
    assert!(
        report.releases.iter().any(|r| r.kept_any("workspace")),
        "and the release says so"
    );

    // Success deletes it.
    let dir = RunDir::new("delete-on-success");
    let mut b = GraphBuilder::new();
    b.add_node(
        "fine",
        scope,
        StepRef::new(PROCESS_KIND, script("echo transient > transient.txt")),
    );
    let graph = b.build();
    let workspace = dir.workspace();
    let report = host_driver_with(
        graph,
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()).with_retention(Retention::OnFailure),
    )
    .await_run()
    .await;
    assert_eq!(report.status, RunStatus::Success);
    assert!(
        !workspace.exists(),
        "a successful scope's workspace is removed"
    );
    assert!(
        report
            .releases
            .iter()
            .all(executor::ReleaseReport::is_clean)
    );
}

/// Two scopes get two workspaces, and each is released on its own.
#[tokio::test]
async fn scopes_get_their_own_workspaces() {
    let dir = RunDir::new("two-scopes");
    let mut b = GraphBuilder::bare();
    let first = b.add_scope(ir::Scope::new(ScopeId::new(0)));
    let second = b.add_scope(ir::Scope::new(ScopeId::new(0)));
    let a = b.add_node(
        "a",
        first,
        StepRef::new(PROCESS_KIND, script("echo one > mine.txt; pwd")),
    );
    let c = b.add_node(
        "c",
        second,
        StepRef::new(PROCESS_KIND, script("test ! -f mine.txt && echo isolated")),
    );
    b.link(a, c);
    let graph = b.build();
    validate(&graph).expect("valid");

    let report = host_driver_with(
        graph,
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path()).with_retention(Retention::Always),
    )
    .await_run()
    .await;

    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(log_lines(&report).iter().any(|l| l == "isolated"));
    assert!(dir.workspace_of(ScopeId::new(0)).join("mine.txt").exists());
    assert!(!dir.workspace_of(ScopeId::new(1)).join("mine.txt").exists());
}

/// Scope env reaches the process.
#[tokio::test]
async fn scope_env_reaches_the_process() {
    let dir = RunDir::new("scope-env");
    let mut b = GraphBuilder::bare();
    let mut scope = ir::Scope::new(ScopeId::new(0));
    scope.env = env(&[("DEPLOY_REGION", "eu-west-1")]);
    let scope = b.add_scope(scope);
    b.add_node(
        "show",
        scope,
        StepRef::new(PROCESS_KIND, script(r#"echo "region is $DEPLOY_REGION""#)),
    );
    let graph = b.build();

    let report = host_driver(graph, &dir).await_run().await;
    assert_eq!(report.status, RunStatus::Success);
    assert!(
        log_lines(&report)
            .iter()
            .any(|l| l == "region is eu-west-1"),
        "{:?}",
        log_lines(&report)
    );
}

/// A step's `working_dir` is relative to the workspace root.
#[tokio::test]
async fn a_step_can_run_in_a_subdirectory() {
    let dir = RunDir::new("working-dir");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let make = b.add_node(
        "make",
        scope,
        StepRef::new(PROCESS_KIND, script("mkdir -p nested/deep")),
    );
    let inside = b.add_node(
        "inside",
        scope,
        StepRef::new(
            PROCESS_KIND,
            script_with(
                "pwd | rev | cut -d/ -f1-2 | rev",
                &json!({ "working_dir": "nested/deep" }),
            ),
        ),
    );
    b.link(make, inside);
    let graph = b.build();

    let report = host_driver_with(
        graph,
        &dir,
        MapSecrets::empty(),
        RunConfig::new(dir.path())
            .with_retention(Retention::Never)
            .with_grace(Duration::from_secs(1)),
    )
    .await_run()
    .await;
    assert_eq!(report.status, RunStatus::Success);
    assert!(
        log_lines(&report).iter().any(|l| l == "nested/deep"),
        "{:?}",
        log_lines(&report)
    );
}
