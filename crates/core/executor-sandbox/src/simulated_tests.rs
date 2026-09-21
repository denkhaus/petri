//! A dry run's router acquires every scope on the simulated provider and
//! reaches for no plugin, and a real router leaves a simulated lease alone.
//! The plugin-free proof through the binary, with an empty `PATH` and no
//! `PETRI_SANDBOX_*` variable, is `petri-cli`'s `dry_run_cli` test.

use std::path::Path;
use std::sync::Arc;

use executor::{
    AcquireContext, Executor, ProcessSpec, Retention, SandboxLeaseId, ScopeOutcome, ScopeSpec,
};
use testkit::RunDir;

use crate::{LeaseLedger, LeaseState, MemoryLedger, RoutingExecutor, SIMULATED_KIND};

const LEASE: SandboxLeaseId = SandboxLeaseId::new(7);

#[tokio::test]
async fn a_simulated_router_acquires_every_target_without_a_plugin() {
    let dir = RunDir::new("simulated-router");
    let router = RoutingExecutor::simulated(dir.path().to_path_buf());
    let ledger = Arc::new(MemoryLedger::default());
    router.set_ledger(ledger.clone());
    let ctx = AcquireContext::bare().with_lease(LEASE);

    let host = ScopeSpec::new(ir::ScopeId::new(0), "host-scope");
    assert_eq!(router.provider_kind_for(&host.runtime), SIMULATED_KIND);
    let handle = router
        .acquire(&host, &ctx)
        .await
        .expect("a host-process scope is simulated");
    assert_eq!(handle.sandbox().provider, SIMULATED_KIND);
    assert!(
        handle
            .sandbox()
            .working_directory
            .starts_with("/simulated/"),
        "{:?}",
        handle.sandbox()
    );
    let env = handle.exec();
    assert!(!env.shares_host_filesystem());
    assert!(
        env.host_address().is_err(),
        "nothing runs here, so no route back"
    );
    // A spawn is lazy: the refusal reaches the step when it waits.
    let mut process = env
        .spawn(ProcessSpec::new("true", &[]))
        .await
        .expect("the job is handed to the sandbox");
    let waited = process.wait().await;
    assert!(
        waited
            .as_ref()
            .is_err_and(|error| error.to_string().contains("runs no process")),
        "{waited:?}"
    );
    assert_eq!(env.read_file(Path::new("a.txt")).await.expect("read"), None);
    assert!(env.write_file(Path::new("a.txt"), b"x").await.is_err());
    assert!(
        !dir.path().join("host-registry").exists(),
        "no host plugin ran"
    );
    assert!(
        !dir.path().join("scopes").exists(),
        "no workspace was created"
    );

    let record = ledger
        .lookup(LEASE)
        .await
        .expect("ledger")
        .expect("recorded");
    assert_eq!(record.state, LeaseState::Live);
    assert_eq!(record.provider.as_deref(), Some(SIMULATED_KIND));

    let container = ScopeSpec::new(ir::ScopeId::new(1), "container-scope")
        .with_runtime(ir::RuntimeSpec::container("ghcr.io/example/job:1"));
    assert_eq!(router.provider_kind_for(&container.runtime), SIMULATED_KIND);
    let second = router
        .acquire(
            &container,
            &AcquireContext::bare().with_lease(SandboxLeaseId::new(8)),
        )
        .await
        .expect("a container scope is simulated too");
    assert_eq!(second.sandbox().provider, SIMULATED_KIND);
    assert!(second.container_runner().is_some());

    let report = router.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    let report = router.release(second, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    let ended = router.release_lease(LEASE, ScopeOutcome::Succeeded).await;
    assert!(ended.is_clean(), "{ended:?}");
    let record = ledger
        .lookup(LEASE)
        .await
        .expect("ledger")
        .expect("recorded");
    assert_eq!(record.state, LeaseState::Deleted);
    router.shutdown().await;
}

#[tokio::test]
async fn a_real_router_leaves_a_simulated_lease_alone() {
    let dir = RunDir::new("simulated-lease-real-router");
    let router = RoutingExecutor::local(dir.path().to_path_buf(), Retention::Always);
    let ledger = Arc::new(MemoryLedger::default());
    ledger
        .allocating(LEASE, SIMULATED_KIND, "simulated:fixed")
        .await
        .expect("record");
    ledger.live(LEASE, "petri-run-l7").await.expect("record");
    router.set_ledger(ledger.clone());

    // Prune: nothing was ever on a provider, so nothing is deleted and no
    // plugin is looked for.
    let deleted = router
        .delete_recorded(LEASE, "ws")
        .await
        .expect("a simulated lease prunes clean");
    assert!(deleted.is_empty());
    // Release: the lease is reported, not touched, and no plugin is launched.
    let report = router.release_lease(LEASE, ScopeOutcome::Succeeded).await;
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("simulated by a dry run")),
        "{report:?}"
    );
    let record = ledger
        .lookup(LEASE)
        .await
        .expect("ledger")
        .expect("recorded");
    assert_eq!(record.state, LeaseState::Live);
    router.shutdown().await;
}

#[tokio::test]
async fn a_simulated_router_never_launches_the_host_plugin() {
    let dir = RunDir::new("simulated-no-host-plugin");
    let router = RoutingExecutor::simulated(dir.path().to_path_buf());
    let ledger = Arc::new(MemoryLedger::default());
    ledger
        .allocating(LEASE, "host", "host:somewhere")
        .await
        .expect("record");
    ledger.live(LEASE, "petri-run-l7").await.expect("record");
    router.set_ledger(ledger.clone());
    let report = router.release_lease(LEASE, ScopeOutcome::Succeeded).await;
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("launches no provider plugin")),
        "{report:?}"
    );
    assert!(!dir.path().join("host-registry").exists());
}
