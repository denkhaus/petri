//! Shared host actions keep their caller's helper alive and their env separate.

use std::sync::Arc;

use executor::{
    AcquireContext, ContainerRunner, Executor as _, OneShotContainer, Retention, SandboxLeaseId,
    ScopeOutcome, ScopeSpec, WorkspaceId,
};
use executor_sandbox::{LeaseLedger, MemoryLedger, RoutingExecutor};
use ir::ScopeId;
use testkit::{RunDir, container_id, is_docker_ready, list_containers, recorded_run_id};

async fn action_env(runner: &dyn ContainerRunner) -> String {
    let mut process = runner
        .run(OneShotContainer::registry("alpine:3.20").with_args(&[
            "sh",
            "-c",
            "printf '%s\\n' \"$SCOPE_VALUE\"",
        ]))
        .await
        .expect("action");
    let mut lines = process.lines().expect("lines");
    let mut output = Vec::new();
    while let Some(line) = lines.recv().await {
        output.push(line.line);
    }
    assert!(process.wait().await.expect("wait").is_success());
    output.join("\n")
}

#[tokio::test]
async fn inherited_host_scopes_share_the_helper_and_keep_their_own_env() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("host-actions-inherited");
    let router = RoutingExecutor::local(dir.path(), Retention::Always);
    let lease = SandboxLeaseId::new(7);
    let ledger = Arc::new(MemoryLedger::default());
    ledger.allocating(lease, "host", "host").expect("record");
    ledger.live(lease, "shared").expect("record");
    router.set_ledger(ledger);
    let ctx = AcquireContext::bare().with_lease(lease);
    let mut parent_spec =
        ScopeSpec::new(ScopeId::new(0), "parent").with_workspace_id(WorkspaceId::new("shared"));
    parent_spec
        .env
        .insert("SCOPE_VALUE".into(), "parent".into());
    let parent = router.acquire(&parent_spec, &ctx).await.expect("parent");
    let parent_runner = parent.container_runner().expect("runner");
    assert_eq!(action_env(&*parent_runner).await, "parent");
    let name = format!("petri-{}-a-shared", recorded_run_id(dir.path()));
    let original = container_id(&name).await.expect("action host");

    let mut child_spec =
        ScopeSpec::new(ScopeId::new(1), "child").with_workspace_id(WorkspaceId::new("shared"));
    child_spec.env.insert("SCOPE_VALUE".into(), "child".into());
    let child = router.acquire(&child_spec, &ctx).await.expect("child");
    assert_eq!(
        container_id(&name).await.as_deref(),
        Some(original.as_str())
    );
    let child_runner = child.container_runner().expect("runner");
    assert_eq!(action_env(&*child_runner).await, "child");
    assert_eq!(action_env(&*parent_runner).await, "parent");
    assert_eq!(
        container_id(&name).await.as_deref(),
        Some(original.as_str())
    );

    assert!(
        router
            .release(child, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    assert!(
        router
            .release(parent, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    assert_eq!(
        container_id(&name).await.as_deref(),
        Some(original.as_str())
    );
    let report = router.release_lease(lease, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(report.released_any("action host"));
    let prefix = format!("petri-{}-", recorded_run_id(dir.path()));
    assert!(list_containers(&prefix).await.is_empty());
}

#[tokio::test]
async fn releasing_a_host_lease_needs_no_container_provider() {
    let dir = RunDir::new("host-actions-no-plugin");
    let router = RoutingExecutor::local_with_dev(dir.path(), Retention::Never, false);
    let ledger = Arc::new(MemoryLedger::default());
    let lease = SandboxLeaseId::new(0);
    ledger.allocating(lease, "host", "host").expect("record");
    ledger.live(lease, "scope-0").expect("record");
    router.set_ledger(ledger);
    let scope = ScopeSpec::new(ScopeId::new(0), "scope-0");
    let env = router
        .acquire(&scope, &AcquireContext::bare().with_lease(lease))
        .await
        .expect("host acquire");
    assert!(
        router
            .release(env, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    let report = router.release_lease(lease, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(!dir.path().join(executor_sandbox::RUN_ID_FILE).exists());
}
