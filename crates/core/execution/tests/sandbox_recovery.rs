//! Interrupt ledger transitions and reopen the durable store over the same
//! Docker resources. No test reconstructs the expected record by hand.

use std::path::Path;
use std::sync::{Arc, Mutex};

use execution::{
    InvocationId, ResourceLedger, ResourceStore, SandboxAllocationKey, SandboxResourceRecord,
};
use executor::{AcquireContext, Executor, Retention, SandboxLeaseId, ScopeOutcome, ScopeSpec};
use executor_sandbox::{
    LeaseLedger, LeaseRecord, LeaseState, LedgerError, PendingIntent, RoutingExecutor,
};
use ir::{RuntimeSpec, ScopeId};
use testkit::{RunDir, container_id, container_write, is_docker_ready, sandbox_name};

const LEASE: SandboxLeaseId = SandboxLeaseId::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crash {
    Reserved,
    Created,
    StopIntent,
    Stopped,
    DeleteIntent,
    Deleted,
}

struct InterruptedLedger {
    inner: ResourceLedger,
    crash: Mutex<Option<Crash>>,
}

impl InterruptedLedger {
    fn interrupt(&self, at: Crash) -> Result<(), LedgerError> {
        let mut crash = self
            .crash
            .lock()
            .expect("the test crash control is not poisoned");
        if *crash == Some(at) {
            *crash = None;
            return Err(LedgerError(format!("simulated crash at {at:?}")));
        }
        Ok(())
    }
}

impl LeaseLedger for InterruptedLedger {
    fn lookup(&self, lease: SandboxLeaseId) -> Result<Option<LeaseRecord>, LedgerError> {
        self.inner.lookup(lease)
    }

    fn allocating(
        &self,
        lease: SandboxLeaseId,
        provider: &str,
        fingerprint: &str,
    ) -> Result<(), LedgerError> {
        self.inner.allocating(lease, provider, fingerprint)?;
        self.interrupt(Crash::Reserved)
    }

    fn live(&self, lease: SandboxLeaseId, resource_id: &str) -> Result<(), LedgerError> {
        self.interrupt(Crash::Created)?;
        self.inner.live(lease, resource_id)
    }

    fn pending(&self, lease: SandboxLeaseId, intent: PendingIntent) -> Result<(), LedgerError> {
        self.inner.pending(lease, intent)?;
        self.interrupt(match intent {
            PendingIntent::Stop => Crash::StopIntent,
            PendingIntent::Delete => Crash::DeleteIntent,
        })
    }

    fn stopped(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.interrupt(Crash::Stopped)?;
        self.inner.stopped(lease)
    }

    fn deleted(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.interrupt(Crash::Deleted)?;
        self.inner.deleted(lease)
    }
}

struct Fixture {
    directory: RunDir,
    scope:     ScopeSpec,
}

impl Fixture {
    fn new() -> Self {
        let directory = RunDir::new("sandbox-ledger-recovery");
        let scope = ScopeSpec::new(ScopeId::new(0), "scope-0")
            .with_runtime(RuntimeSpec::container("alpine:3.20"));
        ResourceStore::load(directory.path().join("resources"))
            .expect("the test resource store opens")
            .ensure_record(
                SandboxAllocationKey {
                    invocation: InvocationId::ROOT,
                    scope:      engine::ScopeIdentity::Declared(scope.id),
                },
                "docker",
                scope.workspace_id.clone(),
                scope.runtime.clone(),
                None,
            )
            .expect("the initial lease is reserved");
        Self { directory, scope }
    }

    fn store(&self) -> ResourceStore {
        ResourceStore::load(self.directory.path().join("resources"))
            .expect("the persisted resource store reopens")
    }

    fn router(&self, retention: Retention, crash: Option<Crash>) -> RoutingExecutor {
        let ledger = ResourceLedger::new(Arc::new(Mutex::new(self.store())));
        let router = RoutingExecutor::local(self.directory.path(), retention);
        router.set_ledger(Arc::new(InterruptedLedger {
            inner: ledger,
            crash: Mutex::new(crash),
        }));
        router
    }

    fn record(&self) -> SandboxResourceRecord {
        self.store()
            .resolve(LEASE)
            .expect("the fixture lease is recorded")
            .clone()
    }

    async fn cleanup(&self, router: &RoutingExecutor) {
        router
            .delete_recorded(LEASE, self.scope.workspace_id.as_str())
            .await
            .expect("the test sandbox is deleted");
        assert_eq!(self.record().state, LeaseState::Deleted);
        assert_eq!(self.record().pending, None);
        assert!(
            container_id(&sandbox_name(self.directory.path(), LEASE.raw()))
                .await
                .is_none()
        );
    }
}

#[tokio::test]
async fn allocation_crashes_recover_without_replacing_the_workspace() {
    if !is_docker_ready().await {
        return;
    }
    for crash in [Crash::Reserved, Crash::Created] {
        let fixture = Fixture::new();
        let router = fixture.router(Retention::Never, Some(crash));
        let ctx = AcquireContext::bare().with_lease(LEASE);
        assert!(router.acquire(&fixture.scope, &ctx).await.is_err());
        assert_eq!(fixture.record().state, LeaseState::Allocating);
        let name = sandbox_name(fixture.directory.path(), LEASE.raw());
        let created = container_id(&name).await;
        if crash == Crash::Created {
            assert!(created.is_some());
            assert!(container_write(&name, "/workspace/before", "preserved").await);
        } else {
            assert!(created.is_none());
        }
        drop(router);

        let resumed = fixture.router(Retention::Never, None);
        let handle = resumed.acquire(&fixture.scope, &ctx).await.unwrap();
        if let Some(created) = created {
            assert_eq!(container_id(&name).await, Some(created));
            assert_eq!(
                handle
                    .exec()
                    .read_file(Path::new("before"))
                    .await
                    .unwrap()
                    .unwrap(),
                b"preserved"
            );
        }
        assert_eq!(fixture.record().state, LeaseState::Live);
        assert!(
            resumed
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        fixture.cleanup(&resumed).await;
    }
}

#[tokio::test]
async fn interrupted_stop_resumes_the_same_sandbox_and_workspace() {
    if !is_docker_ready().await {
        return;
    }
    for crash in [Crash::StopIntent, Crash::Stopped] {
        let fixture = Fixture::new();
        let router = fixture.router(Retention::Always, Some(crash));
        let ctx = AcquireContext::bare().with_lease(LEASE);
        let handle = router.acquire(&fixture.scope, &ctx).await.unwrap();
        handle
            .exec()
            .write_file(Path::new("before"), b"preserved")
            .await
            .unwrap();
        let created = fixture.record().resource_id;
        assert!(
            router
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert!(
            !router
                .release_lease(LEASE, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert_eq!(fixture.record().pending, Some(PendingIntent::Stop));
        drop(router);

        let resumed = fixture.router(Retention::Never, None);
        let handle = resumed.acquire(&fixture.scope, &ctx).await.unwrap();
        assert_eq!(fixture.record().resource_id, created);
        assert_eq!(fixture.record().pending, None);
        assert_eq!(
            handle
                .exec()
                .read_file(Path::new("before"))
                .await
                .unwrap()
                .unwrap(),
            b"preserved"
        );
        assert!(
            resumed
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        fixture.cleanup(&resumed).await;
    }
}

#[tokio::test]
async fn interrupted_delete_finishes_as_a_tombstone() {
    if !is_docker_ready().await {
        return;
    }
    for crash in [Crash::DeleteIntent, Crash::Deleted] {
        let fixture = Fixture::new();
        let router = fixture.router(Retention::Never, Some(crash));
        let ctx = AcquireContext::bare().with_lease(LEASE);
        let handle = router.acquire(&fixture.scope, &ctx).await.unwrap();
        assert!(
            router
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert!(
            !router
                .release_lease(LEASE, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert_eq!(fixture.record().pending, Some(PendingIntent::Delete));
        drop(router);

        let resumed = fixture.router(Retention::Never, None);
        assert!(resumed.acquire(&fixture.scope, &ctx).await.is_err());
        assert_eq!(fixture.record().state, LeaseState::Deleted, "{crash:?}");
        assert_eq!(fixture.record().pending, None);
        fixture.cleanup(&resumed).await;
    }
}

#[tokio::test]
async fn recovery_refuses_a_changed_fingerprint_or_a_lost_workspace() {
    if !is_docker_ready().await {
        return;
    }
    let fixture = Fixture::new();
    let router = fixture.router(Retention::Never, None);
    let ctx = AcquireContext::bare().with_lease(LEASE);
    let handle = router.acquire(&fixture.scope, &ctx).await.unwrap();
    assert!(
        router
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    let original = fixture.record();
    drop(router);
    fixture
        .store()
        .update(LEASE, |record| {
            record.fingerprint = Some("another-daemon".into());
        })
        .unwrap();
    let resumed = fixture.router(Retention::Never, None);
    let error = resumed.acquire(&fixture.scope, &ctx).await.unwrap_err();
    assert!(error.to_string().contains("another-daemon"), "{error}");
    drop(resumed);

    fixture
        .store()
        .update(LEASE, |record| {
            record.fingerprint.clone_from(&original.fingerprint);
        })
        .unwrap();
    let resumed = fixture.router(Retention::Never, None);
    fixture.cleanup(&resumed).await;
    drop(resumed);
    // A resource lost outside Petri leaves its last confirmed live record.
    fixture
        .store()
        .update(LEASE, |record| *record = original)
        .unwrap();
    let resumed = fixture.router(Retention::Never, None);
    let error = resumed.acquire(&fixture.scope, &ctx).await.unwrap_err();
    assert!(error.to_string().contains("workspace was lost"), "{error}");
    fixture.cleanup(&resumed).await;
}

#[tokio::test]
async fn release_reconciles_an_allocation_without_a_recorded_resource_id() {
    if !is_docker_ready().await {
        return;
    }
    for crash in [Crash::Reserved, Crash::Created] {
        for retention in [Retention::Never, Retention::Always] {
            let fixture = Fixture::new();
            let router = fixture.router(retention, Some(crash));
            let ctx = AcquireContext::bare().with_lease(LEASE);
            assert!(router.acquire(&fixture.scope, &ctx).await.is_err());
            assert!(fixture.record().resource_id.is_none());
            router.shutdown().await;
            drop(router);

            let resumed = fixture.router(retention, None);
            let report = resumed.release_lease(LEASE, ScopeOutcome::Failed).await;
            assert!(report.is_clean(), "{report:?}");
            let name = sandbox_name(fixture.directory.path(), LEASE.raw());
            if crash == Crash::Created && retention == Retention::Always {
                assert_eq!(fixture.record().state, LeaseState::Stopped);
                assert!(container_id(&name).await.is_some());
                assert!(!testkit::container_is_running(&name).await);
            } else {
                assert_eq!(fixture.record().state, LeaseState::Deleted);
                assert!(container_id(&name).await.is_none());
            }
            fixture.cleanup(&resumed).await;
            resumed.shutdown().await;
        }
    }
}

#[tokio::test]
async fn release_finishes_a_pending_delete_even_when_retention_changes() {
    if !is_docker_ready().await {
        return;
    }
    for crash in [Crash::DeleteIntent, Crash::Deleted] {
        let fixture = Fixture::new();
        let router = fixture.router(Retention::Never, Some(crash));
        let ctx = AcquireContext::bare().with_lease(LEASE);
        let handle = router.acquire(&fixture.scope, &ctx).await.unwrap();
        assert!(
            router
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert!(
            !router
                .release_lease(LEASE, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert_eq!(fixture.record().pending, Some(PendingIntent::Delete));
        router.shutdown().await;
        drop(router);

        let resumed = fixture.router(Retention::Always, None);
        let report = resumed.release_lease(LEASE, ScopeOutcome::Failed).await;
        assert!(report.is_clean(), "{report:?}");
        fixture.cleanup(&resumed).await;
        resumed.shutdown().await;
    }
}
