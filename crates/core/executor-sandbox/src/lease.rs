//! Sandboxes by lease: one live handle per durable lease, shared by every
//! holder, fenced only during crash recovery.
//!
//! A container sandbox lives as long as the lease that names its workspace.
//! The coordinator allocates the lease and hands it to each execution that
//! runs in the sandbox; this manager keeps the one live handle and a holder
//! count. A second acquire on a live lease — a restarted execution, an
//! inherited nested invocation, concurrent scopes over one workspace —
//! reuses the handle and never stops the sandbox. Per-execution release only
//! drops a holder. The sandbox is stopped when its invocation releases the
//! lease and kept or deleted by retention; `petri sandbox prune` deletes
//! kept ones later.
//!
//! # Recovery
//!
//! When no live holder exists — a fresh process over a run dir, or a plugin
//! generation change that invalidated every old handle — acquire
//! reconciles: it lists the provider by the workspace label, attaches the
//! one recorded match, stops it once (ending whatever a dead execution or a
//! dead plugin left running), and starts it once before any holder resumes.
//! More than one match is an error, never an arbitrary choice; a confirmed
//! record whose resource is missing is an error, never a silent replacement,
//! because its workspace was lost. A create happens only when reconciliation
//! finds nothing.
//!
//! # The ledger
//!
//! Every transition is written to a [`LeaseLedger`] before the provider call
//! that makes it true (`allocating` before create, `pending: stop` before
//! stop, `pending: delete` before delete) and confirmed only after the
//! provider succeeds. Recovery repeats a pending idempotent operation. The
//! coordinator's resource store is the durable ledger; a standalone driver
//! uses an in-memory one and releases its sandbox at scope release.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use executor::{EnvError, ReleaseReport, Retention, SandboxLeaseId, ScopeOutcome};
use sandbox_driver::{Error as DriverError, Sandbox, SandboxFilter, SandboxId, SandboxSpec};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio::task::JoinSet;

use crate::plugin::ProviderSource;
use crate::run::{RunIdentity, WORKSPACE_LABEL};
use crate::{BACKEND, acquire_failed};

/// Where a lease's sandbox stands, durably.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// Reserved; the provider may or may not have created the resource.
    #[default]
    Allocating,
    /// The resource exists and was last known running.
    Live,
    /// The resource exists, stopped; its workspace is kept.
    Stopped,
    /// The resource was deleted. The record stays as a tombstone.
    Deleted,
}

/// An operation written down before it is asked of the provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingIntent {
    Stop,
    Delete,
}

/// The durable facts about one lease the manager reads and writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRecord {
    pub state:       LeaseState,
    pub pending:     Option<PendingIntent>,
    /// The real provider kind, once known.
    pub provider:    Option<SmolStr>,
    /// The provider's resource id, once known.
    pub resource_id: Option<SmolStr>,
    /// The non-secret provider fingerprint recorded at allocation.
    pub fingerprint: Option<SmolStr>,
}

/// Why a ledger operation failed.
#[derive(Debug, thiserror::Error)]
#[error("sandbox lease ledger: {0}")]
pub struct LedgerError(pub String);

/// The durable record of every lease, as the manager needs it.
pub trait LeaseLedger: Send + Sync {
    fn lookup(&self, lease: SandboxLeaseId) -> Result<Option<LeaseRecord>, LedgerError>;

    /// Records that the provider is about to be asked to create the
    /// resource, and which provider on which backend.
    fn allocating(
        &self,
        lease: SandboxLeaseId,
        provider: &str,
        fingerprint: &str,
    ) -> Result<(), LedgerError>;

    /// Records the resource as created (or found) and running.
    fn live(&self, lease: SandboxLeaseId, resource_id: &str) -> Result<(), LedgerError>;

    /// Records an intent before the provider call. The intent stays until
    /// the matching confirmation.
    fn pending(&self, lease: SandboxLeaseId, intent: PendingIntent) -> Result<(), LedgerError>;

    fn stopped(&self, lease: SandboxLeaseId) -> Result<(), LedgerError>;

    fn deleted(&self, lease: SandboxLeaseId) -> Result<(), LedgerError>;
}

/// A ledger that forgets everything when the process ends: for a driver
/// with no coordinator, whose sandboxes end with their scopes.
#[derive(Default)]
pub struct MemoryLedger {
    records: Mutex<HashMap<SandboxLeaseId, LeaseRecord>>,
}

impl MemoryLedger {
    fn update(&self, lease: SandboxLeaseId, update: impl FnOnce(&mut LeaseRecord)) {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let record = records.entry(lease).or_insert_with(|| LeaseRecord {
            state:       LeaseState::Allocating,
            pending:     None,
            provider:    None,
            resource_id: None,
            fingerprint: None,
        });
        update(record);
    }
}

impl LeaseLedger for MemoryLedger {
    fn lookup(&self, lease: SandboxLeaseId) -> Result<Option<LeaseRecord>, LedgerError> {
        Ok(self
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&lease)
            .cloned())
    }

    fn allocating(
        &self,
        lease: SandboxLeaseId,
        provider: &str,
        fingerprint: &str,
    ) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Allocating;
            record.provider = Some(SmolStr::new(provider));
            record.fingerprint = Some(SmolStr::new(fingerprint));
        });
        Ok(())
    }

    fn live(&self, lease: SandboxLeaseId, resource_id: &str) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Live;
            record.pending = None;
            record.resource_id = Some(SmolStr::new(resource_id));
        });
        Ok(())
    }

    fn pending(&self, lease: SandboxLeaseId, intent: PendingIntent) -> Result<(), LedgerError> {
        self.update(lease, |record| record.pending = Some(intent));
        Ok(())
    }

    fn stopped(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Stopped;
            record.pending = None;
        });
        Ok(())
    }

    fn deleted(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Deleted;
            record.pending = None;
        });
        Ok(())
    }
}

/// The live handle a lease currently has.
struct LiveSandbox {
    sandbox:    Arc<dyn Sandbox>,
    generation: u64,
}

/// Per-lease state, serialized by one lock so allocation and recovery for
/// one lease never race.
#[derive(Default)]
struct LeaseSlot {
    live:    Option<LiveSandbox>,
    holders: usize,
}

/// What an acquire needs to know beyond the lease.
pub(crate) struct LeaseRequest<'a> {
    pub lease:        SandboxLeaseId,
    /// The workspace the lease names, for the reconcile label.
    pub workspace_id: &'a str,
    /// A standalone acquisition has no coordinator to release its lease.
    pub standalone:   bool,
}

/// One live handle and a holder count per lease, over one provider source.
pub struct SandboxLeaseManager {
    source:   Arc<dyn ProviderSource>,
    ledger:   Arc<dyn LeaseLedger>,
    identity: Arc<RunIdentity>,
    leases:   Mutex<HashMap<SandboxLeaseId, Arc<AsyncMutex<LeaseSlot>>>>,
    cleanup:  Mutex<JoinSet<()>>,
}

impl SandboxLeaseManager {
    pub fn new(
        source: Arc<dyn ProviderSource>,
        ledger: Arc<dyn LeaseLedger>,
        identity: Arc<RunIdentity>,
    ) -> Self {
        Self {
            source,
            ledger,
            identity,
            leases: Mutex::new(HashMap::new()),
            cleanup: Mutex::new(JoinSet::new()),
        }
    }

    pub fn source(&self) -> &Arc<dyn ProviderSource> {
        &self.source
    }

    fn slot(&self, lease: SandboxLeaseId) -> Arc<AsyncMutex<LeaseSlot>> {
        Arc::clone(
            self.leases
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(lease)
                .or_default(),
        )
    }

    fn ledger_failed(error: &LedgerError) -> EnvError {
        EnvError::backend(BACKEND, "lease", error.to_string())
    }

    /// The sandbox for `request.lease`, allocating, attaching, or reusing
    /// as the lease's state requires, and counting the caller as a holder.
    /// `build_spec` produces the create spec, given the labels every Petri
    /// sandbox carries.
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        request: LeaseRequest<'_>,
        build_spec: impl FnOnce(&[(String, String)]) -> Result<SandboxSpec, EnvError>,
    ) -> Result<AcquiredSandbox, EnvError> {
        // An owned guard: a create hands it to its task, so an acquire that
        // is dropped mid-create still keeps the lease locked until the
        // create has settled one way or the other.
        let mut slot = self.slot(request.lease).lock_owned().await;
        let (provider, generation) = self.source.current().await?;
        if let Some(live) = &slot.live {
            if live.generation == generation {
                let sandbox = Arc::clone(&live.sandbox);
                slot.holders += 1;
                return Ok(self.acquired(&request, sandbox));
            }
            // A new plugin generation: every old handle is dead, and the
            // sandbox is fenced below before anyone resumes on it.
            tracing::warn!(
                lease = request.lease.raw(),
                old_generation = live.generation,
                generation,
                "sandbox plugin generation changed; fencing the lease's sandbox"
            );
            slot.live = None;
        }

        let record = self
            .ledger
            .lookup(request.lease)
            .map_err(|error| Self::ledger_failed(&error))?;
        let run_id = self.identity.run_id().await?;
        let workspace_label = self.identity.workspace_label(request.workspace_id).await?;
        let labels = self
            .identity
            .labels(request.lease, request.workspace_id)
            .await?;

        let sandbox = match record {
            Some(LeaseRecord {
                state: LeaseState::Deleted,
                ..
            }) => {
                return Err(EnvError::backend(
                    BACKEND,
                    "acquire",
                    format!(
                        "sandbox lease {} was deleted; its workspace is gone",
                        request.lease
                    ),
                ));
            }
            Some(record) => {
                self.check_fingerprint(request.lease, &record)?;
                let matches = list_by_label(&*provider, &workspace_label).await?;
                match matches.len() {
                    0 if record.state == LeaseState::Allocating => {
                        // The record was reserved but no resource exists:
                        // the create never happened or never completed.
                        return self
                            .create(&provider, &request, &labels, build_spec, slot, generation)
                            .await;
                    }
                    0 => {
                        return Err(EnvError::backend(
                            BACKEND,
                            "acquire",
                            format!(
                                "sandbox lease {} is recorded {:?} but no sandbox carries \
                                 {WORKSPACE_LABEL}={workspace_label} on the provider; its \
                                 workspace was lost and Petri will not replace it silently",
                                request.lease, record.state
                            ),
                        ));
                    }
                    1 => {
                        let found = &matches[0];
                        if let Some(recorded) = &record.resource_id
                            && recorded.as_str() != found.as_str()
                        {
                            return Err(EnvError::backend(
                                BACKEND,
                                "acquire",
                                format!(
                                    "sandbox lease {} records resource {recorded} but the \
                                     provider holds {found} for its workspace",
                                    request.lease
                                ),
                            ));
                        }
                        self.recover(&*provider, request.lease, found, record.pending)
                            .await?
                    }
                    count => {
                        return Err(EnvError::backend(
                            BACKEND,
                            "acquire",
                            format!(
                                "{count} sandboxes carry {WORKSPACE_LABEL}={workspace_label}; \
                                 Petri does not choose one arbitrarily"
                            ),
                        ));
                    }
                }
            }
            None => {
                // Nothing recorded: reconcile by label anyway, so a create
                // that a crash left unrecorded is found, not duplicated.
                let matches = list_by_label(&*provider, &workspace_label).await?;
                match matches.len() {
                    0 => {
                        return self
                            .create(&provider, &request, &labels, build_spec, slot, generation)
                            .await;
                    }
                    1 => {
                        self.ledger
                            .allocating(
                                request.lease,
                                self.source.kind(),
                                self.source.fingerprint(),
                            )
                            .map_err(|error| Self::ledger_failed(&error))?;
                        self.recover(&*provider, request.lease, &matches[0], None)
                            .await?
                    }
                    count => {
                        return Err(EnvError::backend(
                            BACKEND,
                            "acquire",
                            format!(
                                "{count} sandboxes carry {WORKSPACE_LABEL}={workspace_label} \
                                 (run {run_id}); Petri does not choose one arbitrarily"
                            ),
                        ));
                    }
                }
            }
        };
        slot.live = Some(LiveSandbox {
            sandbox: Arc::clone(&sandbox),
            generation,
        });
        // Older generations can still have holders finishing or abandoning
        // initialization. Their later release must not consume this holder.
        slot.holders += 1;
        Ok(self.acquired(&request, sandbox))
    }

    fn acquired(
        self: &Arc<Self>,
        request: &LeaseRequest<'_>,
        sandbox: Arc<dyn Sandbox>,
    ) -> AcquiredSandbox {
        AcquiredSandbox {
            manager:    self.clone(),
            lease:      request.lease,
            standalone: request.standalone,
            sandbox:    Some(sandbox),
        }
    }

    fn check_fingerprint(
        &self,
        lease: SandboxLeaseId,
        record: &LeaseRecord,
    ) -> Result<(), EnvError> {
        if let Some(recorded) = &record.fingerprint {
            let current = self.source.fingerprint();
            if recorded.as_str() != current {
                return Err(EnvError::backend(
                    BACKEND,
                    "acquire",
                    format!(
                        "sandbox lease {lease} was allocated on `{recorded}` but this run is \
                         configured for `{current}`; a changed DOCKER_HOST, Daytona \
                         organization or target must be restored before the run continues"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Creates the sandbox, recording `allocating` first and `live` after,
    /// so a crash on either side of the create is reconciled by label.
    ///
    /// The create runs in its own task, which owns the lease's lock: an
    /// acquire nobody waited out (a cancelled scope, a sweep aborting the
    /// run) drops this future, and the provider's create — already sent —
    /// still completes, with the lease locked until it has. The task
    /// returns an acquisition guard. Dropping either the task's unread
    /// result or later environment initialization releases that holder.
    async fn create(
        self: &Arc<Self>,
        provider: &Arc<dyn sandbox_driver::SandboxProvider>,
        request: &LeaseRequest<'_>,
        labels: &[(String, String)],
        build_spec: impl FnOnce(&[(String, String)]) -> Result<SandboxSpec, EnvError>,
        mut slot: OwnedMutexGuard<LeaseSlot>,
        generation: u64,
    ) -> Result<AcquiredSandbox, EnvError> {
        let lease = request.lease;
        let standalone = request.standalone;
        self.ledger
            .allocating(lease, self.source.kind(), self.source.fingerprint())
            .map_err(|error| Self::ledger_failed(&error))?;
        let spec = build_spec(labels)?;
        let provider = provider.clone();
        let manager = self.clone();
        let created = tokio::spawn(async move {
            let sandbox = provider
                .create(&spec, None)
                .await
                .map_err(|error| acquire_failed(&error))?;
            let acquired = AcquiredSandbox {
                manager: manager.clone(),
                lease,
                standalone,
                sandbox: Some(sandbox.clone()),
            };
            slot.live = Some(LiveSandbox {
                sandbox: sandbox.clone(),
                generation,
            });
            slot.holders += 1;
            manager
                .ledger
                .live(lease, sandbox.id().as_str())
                .map_err(|error| Self::ledger_failed(&error))?;
            drop(slot);
            Ok::<_, EnvError>(acquired)
        });
        created.await.map_err(|error| {
            EnvError::backend(
                BACKEND,
                "acquire",
                format!("the sandbox create task failed: {error}"),
            )
        })?
    }

    /// Attaches a recorded sandbox and fences it: one stop, then one start.
    async fn recover(
        &self,
        provider: &dyn sandbox_driver::SandboxProvider,
        lease: SandboxLeaseId,
        id: &SandboxId,
        pending: Option<PendingIntent>,
    ) -> Result<Arc<dyn Sandbox>, EnvError> {
        let sandbox = provider
            .attach(id, None)
            .await
            .map_err(|error| acquire_failed(&error))?;
        if pending == Some(PendingIntent::Delete) {
            // The invocation meant to delete it and died before the
            // provider confirmed; finish that, and the lease is gone.
            provider
                .delete(id, None)
                .await
                .map_err(|error| acquire_failed(&error))?;
            self.ledger
                .deleted(lease)
                .map_err(|error| Self::ledger_failed(&error))?;
            return Err(EnvError::backend(
                BACKEND,
                "acquire",
                format!("sandbox lease {lease} was being deleted; its workspace is gone"),
            ));
        }
        tracing::info!(lease = lease.raw(), sandbox = %id, "fencing a recorded sandbox");
        sandbox
            .stop()
            .await
            .map_err(|error| acquire_failed(&error))?;
        sandbox
            .start()
            .await
            .map_err(|error| acquire_failed(&error))?;
        self.ledger
            .live(lease, id.as_str())
            .map_err(|error| Self::ledger_failed(&error))?;
        Ok(sandbox)
    }

    /// Drops one holder. Never stops the sandbox: that is the lease
    /// release's job.
    pub async fn release_holder(&self, lease: SandboxLeaseId) -> usize {
        let slot = self.slot(lease);
        let mut slot = slot.lock().await;
        slot.holders = slot.holders.saturating_sub(1);
        slot.holders
    }

    async fn abandon(&self, lease: SandboxLeaseId, standalone: bool) {
        let mut slot = self.slot(lease).lock_owned().await;
        slot.holders = slot.holders.saturating_sub(1);
        if standalone && slot.holders == 0 {
            let report = self
                .release_locked(lease, Retention::Never, ScopeOutcome::Failed, slot)
                .await;
            for problem in report.problems {
                tracing::warn!(
                    lease = lease.raw(),
                    problem,
                    "abandoned sandbox cleanup failed"
                );
            }
        }
    }

    fn abandon_in_background(self: &Arc<Self>, lease: SandboxLeaseId, standalone: bool) {
        let mut cleanup = self.cleanup.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(result) = cleanup.try_join_next() {
            if let Err(error) = result {
                tracing::warn!(%error, "sandbox cleanup task failed");
            }
        }
        let manager = self.clone();
        cleanup.spawn(async move { manager.abandon(lease, standalone).await });
    }

    /// The live handle, if the lease has one in this process.
    pub async fn live(&self, lease: SandboxLeaseId) -> Option<Arc<dyn Sandbox>> {
        let slot = self.slot(lease);
        let slot = slot.lock().await;
        slot.live.as_ref().map(|live| Arc::clone(&live.sandbox))
    }

    /// Ends the lease: stops the sandbox, then keeps it stopped or deletes
    /// it as `retention` decides for `outcome`. Each step is written to the
    /// ledger as an intent first and confirmed after.
    pub async fn release_lease(
        &self,
        lease: SandboxLeaseId,
        retention: Retention,
        outcome: ScopeOutcome,
    ) -> ReleaseReport {
        let slot = self.slot(lease).lock_owned().await;
        self.release_locked(lease, retention, outcome, slot).await
    }

    async fn release_locked(
        &self,
        lease: SandboxLeaseId,
        retention: Retention,
        outcome: ScopeOutcome,
        mut slot: OwnedMutexGuard<LeaseSlot>,
    ) -> ReleaseReport {
        let report = ReleaseReport::default();
        let live = slot.live.take();
        slot.holders = 0;
        let record = match self.ledger.lookup(lease) {
            Ok(record) => record,
            Err(error) => return report.problem(error.to_string()),
        };
        let Some(record) = record else {
            return report;
        };
        if record.state == LeaseState::Deleted {
            return report;
        }
        if let Err(error) = self.check_fingerprint(lease, &record) {
            return report.problem(error.to_string());
        }
        let resource_id = record.resource_id.clone().or_else(|| {
            live.as_ref()
                .map(|live| SmolStr::new(live.sandbox.id().as_str()))
        });
        let Some(resource_id) = resource_id else {
            // Reserved but never created: nothing on the provider to end.
            return report;
        };
        let id = match SandboxId::try_new(resource_id.as_str()) {
            Ok(id) => id,
            Err(error) => return report.problem(error.to_string()),
        };
        let (provider, _) = match self.source.current().await {
            Ok(current) => current,
            Err(error) => return report.problem(format!("sandbox provider unavailable: {error}")),
        };
        let keep = retention.keeps(outcome);
        let intent = if keep {
            PendingIntent::Stop
        } else {
            PendingIntent::Delete
        };
        if let Err(error) = self.ledger.pending(lease, intent) {
            return report.problem(error.to_string());
        }
        if keep {
            let outcome = match live {
                Some(live) => live.sandbox.stop().await,
                None => match provider.attach(&id, None).await {
                    Ok(sandbox) => sandbox.stop().await,
                    Err(DriverError::NotFound { .. }) => Ok(()),
                    Err(error) => Err(error),
                },
            };
            match outcome {
                Ok(()) => {
                    if let Err(error) = self.ledger.stopped(lease) {
                        return report.problem(error.to_string());
                    }
                    report.kept(format!("sandbox {id} (stopped, lease {lease})"))
                }
                Err(error) => report.problem(format!("sandbox {id} stop failed: {error}")),
            }
        } else {
            match provider.delete(&id, None).await {
                Ok(()) => {
                    if let Err(error) = self.ledger.deleted(lease) {
                        return report.problem(error.to_string());
                    }
                    report.released(format!("sandbox {id} (lease {lease})"))
                }
                Err(error) => report.problem(format!("sandbox {id} delete failed: {error}")),
            }
        }
    }

    /// Deletes a recorded sandbox without a live handle: the prune path.
    /// The fingerprint must match, the intent is written first, and the
    /// record becomes a tombstone only after the provider confirms. A
    /// record that never learned its resource id — a crash between create
    /// and the `live` write — is reconciled by the workspace label of
    /// `workspace_id`, the same key recovery uses. The ids deleted come
    /// back; an empty list means nothing was on the provider.
    pub async fn delete_recorded(
        &self,
        lease: SandboxLeaseId,
        workspace_id: &str,
    ) -> Result<Vec<SandboxId>, EnvError> {
        let slot = self.slot(lease);
        let mut slot = slot.lock().await;
        slot.live = None;
        slot.holders = 0;
        let Some(record) = self
            .ledger
            .lookup(lease)
            .map_err(|error| Self::ledger_failed(&error))?
        else {
            return Ok(Vec::new());
        };
        if record.state == LeaseState::Deleted {
            return Ok(Vec::new());
        }
        self.check_fingerprint(lease, &record)?;
        let (provider, _) = self.source.current().await?;
        let ids = if let Some(resource_id) = record.resource_id {
            vec![
                SandboxId::try_new(resource_id.as_str())
                    .map_err(|error| EnvError::backend(BACKEND, "prune", error.to_string()))?,
            ]
        } else {
            let label = self.identity.workspace_label(workspace_id).await?;
            list_by_label(&*provider, &label).await?
        };
        self.ledger
            .pending(lease, PendingIntent::Delete)
            .map_err(|error| Self::ledger_failed(&error))?;
        for id in &ids {
            match provider.delete(id, None).await {
                Ok(()) | Err(DriverError::NotFound { .. }) => {}
                Err(error) => {
                    return Err(EnvError::backend(BACKEND, "prune", error.to_string()));
                }
            }
        }
        self.ledger
            .deleted(lease)
            .map_err(|error| Self::ledger_failed(&error))?;
        Ok(ids)
    }
}

/// Owns a holder until environment initialization hands it to an EnvHandle.
/// An unread create result and an interrupted environment read both drop
/// this guard. The manager owns any asynchronous cleanup that drop starts.
pub(crate) struct AcquiredSandbox {
    manager:    Arc<SandboxLeaseManager>,
    lease:      SandboxLeaseId,
    standalone: bool,
    sandbox:    Option<Arc<dyn Sandbox>>,
}

impl AcquiredSandbox {
    pub(crate) fn sandbox(&self) -> &Arc<dyn Sandbox> {
        self.sandbox
            .as_ref()
            .expect("an acquisition owns its sandbox")
    }

    pub(crate) fn into_sandbox(mut self) -> Arc<dyn Sandbox> {
        self.sandbox
            .take()
            .expect("an acquisition owns its sandbox")
    }

    pub(crate) async fn release(mut self) {
        self.manager.abandon(self.lease, self.standalone).await;
        self.sandbox = None;
    }
}

impl Drop for AcquiredSandbox {
    fn drop(&mut self) {
        if self.sandbox.is_some() {
            self.manager
                .abandon_in_background(self.lease, self.standalone);
        }
    }
}

async fn list_by_label(
    provider: &dyn sandbox_driver::SandboxProvider,
    workspace_label: &str,
) -> Result<Vec<SandboxId>, EnvError> {
    let mut filter = SandboxFilter::default();
    filter
        .labels
        .insert(WORKSPACE_LABEL.to_owned(), workspace_label.to_owned());
    let matches = provider
        .list(&filter)
        .await
        .map_err(|error| acquire_failed(&error))?;
    Ok(matches.into_iter().map(|status| status.id).collect())
}
