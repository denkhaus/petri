//! `petri sandbox prune`: delete every sandbox a finished or abandoned run
//! still holds on its provider.
//!
//! A lease that was never released — a crash, a kill, a `Retention::Always`
//! run — leaves a stopped or running sandbox and its workspace on the
//! provider. The run's resource records say exactly which. Prune takes the
//! run's lease so no coordinator can resume it meanwhile, checks each
//! record's provider fingerprint against the plugin it launches (a changed
//! daemon or account is a configuration error, never a delete on another
//! backend), writes the delete intent before the provider call, and leaves
//! a tombstone after. Host workspaces are directories under the run dir and
//! are not this command's business.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use runtime::Runtime;

use crate::resource::{LeaseState, ResourceLedger, ResourceStore};
use crate::{HOST_PROVIDER, ResourceError, SandboxLeaseId, StoreError, hold_run_lease};

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
    #[error("run `{0}` is held by a live process; stop it first")]
    RunHeld(PathBuf),
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
    let _held = hold_run_lease(&run_dir).map_err(|error| match error {
        StoreError::Leased(path) => PruneError::RunHeld(path),
        other => PruneError::Store(other),
    })?;
    let store = Arc::new(Mutex::new(ResourceStore::load(
        run_dir.join(crate::RESOURCES_DIR),
    )?));
    let run_runtime = rt.prepare_run(&run_dir);
    run_runtime.attach_lease_ledger(Arc::new(ResourceLedger::new(store.clone())));
    let router = run_runtime
        .sandbox_router()
        .cloned()
        .ok_or(PruneError::NoRouter)?;

    let candidates: Vec<(SandboxLeaseId, String)> = store
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .records()
        .filter(|record| record.provider != HOST_PROVIDER)
        .map(|record| (record.lease, record.workspace.as_str().to_owned()))
        .collect();
    let mut report = PruneReport::default();
    for (lease, workspace) in candidates {
        let state = store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resolve(lease)
            .map(|record| record.state)?;
        if state == LeaseState::Deleted {
            report.clean.push(lease);
            continue;
        }
        match router.delete_recorded(lease, &workspace).await {
            Ok(ids) if ids.is_empty() => report.clean.push(lease),
            Ok(ids) => report
                .deleted
                .push((lease, ids.iter().map(ToString::to_string).collect())),
            Err(error) => report.problems.push((lease, error.to_string())),
        }
    }
    run_runtime
        .finish_with_status(ir::RunStatus::Cancelled)
        .await;
    Ok(report)
}
