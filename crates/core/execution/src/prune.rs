//! `petri sandbox prune`: delete every sandbox a finished or abandoned run
//! still holds on its provider.
//!
//! A lease that was never released — a crash, a kill, a `Retention::Always`
//! run — leaves a stopped or running sandbox and its workspace on the
//! provider. The run's resource records say exactly which. Prune opens the
//! run for writing, so no coordinator can resume it meanwhile, checks each
//! record's provider fingerprint against the plugin it launches (a changed
//! daemon or account is a configuration error, never a delete on another
//! backend), writes the delete intent before the provider call, and leaves
//! a tombstone after. Each provider deletes its sandbox's managed workspace,
//! including Host workspaces under the run directory.

use std::sync::Arc;

use runtime::{RunAccess, Runtime};
use tokio::sync::Mutex;

use crate::resource::{ResourceLedger, ResourceStore};
use crate::{ResourceError, SandboxLeaseId, StoreError};

/// What prune did to one run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Leases whose sandboxes were deleted, with the provider's ids.
    pub deleted:  Vec<(SandboxLeaseId, Vec<String>)>,
    /// Leases that needed nothing: already tombstoned, or never allocated.
    pub clean:    Vec<SandboxLeaseId>,
    /// Leases that could not be pruned, with why. Their records keep the
    /// pending intent, so the next prune tries again.
    pub problems: Vec<(SandboxLeaseId, String)>,
}

impl PruneReport {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PruneError {
    /// A live process holds the run: pruning under it would delete the
    /// sandboxes it is using.
    #[error("run {0} is held by a live process; stop it first")]
    RunHeld(String),
    #[error(transparent)]
    Store(StoreError),
    #[error(transparent)]
    Resource(#[from] ResourceError),
    /// The runtime was built with a caller-supplied executor, which keeps no
    /// lease manager this command can reach.
    #[error("this runtime's executor is not the standard router; nothing to prune through")]
    NoRouter,
}

/// Prune the run in the runtime's run dir.
pub async fn prune(rt: &Runtime) -> Result<PruneReport, PruneError> {
    let run_dir = rt.run_options().run_dir.clone();
    let run = rt.prepare_run(&run_dir);
    let logs = run
        .open(RunAccess::Write)
        .await
        .map_err(|error| match error {
            store::StoreError::Leased { locator, .. } => PruneError::RunHeld(locator),
            other => PruneError::Store(other.into()),
        })?;
    let store = Arc::new(Mutex::new(ResourceStore::load(&logs).await?));
    let router = run.sandbox_router().cloned().ok_or(PruneError::NoRouter)?;
    router.set_ledger(Arc::new(ResourceLedger::new(store.clone())));

    let candidates: Vec<(SandboxLeaseId, String)> = store
        .lock()
        .await
        .records()
        .map(|record| (record.lease, record.workspace.as_str().to_owned()))
        .collect();
    let mut report = PruneReport::default();
    for (lease, workspace) in candidates {
        match router.delete_recorded(lease, &workspace).await {
            Ok(ids) if ids.is_empty() => report.clean.push(lease),
            Ok(ids) => report
                .deleted
                .push((lease, ids.iter().map(ToString::to_string).collect())),
            Err(error) => report.problems.push((lease, error.to_string())),
        }
    }
    run.finish().await;
    drop(logs);
    Ok(report)
}
