//! Durable sandbox leases: one record per lease under the run's
//! `resources/`, the crash-safe authority on what the run holds on which
//! provider.
//!
//! A record is reserved before anything exists on a provider, in
//! [`LeaseState::Allocating`]; it becomes `live` with the provider's own
//! resource id only after the provider confirms the create (or recovery
//! finds the one match). Every stop and delete is written down as a
//! [`PendingIntent`] before the provider is asked and confirmed after, so a
//! crash between the two leaves a repeatable intent, never a lie. A deleted
//! lease stays as a tombstone while the run directory exists, so a
//! historical inherited invocation still resolves during replay while new
//! work on the lease is refused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::{fs, io};

use executor::WorkspaceId;
use executor_sandbox::{LeaseLedger, LeaseRecord, LedgerError};
pub use executor_sandbox::{LeaseState, PendingIntent};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::store::write_atomically_with;
use crate::{SandboxAllocationKey, SandboxLeaseId};

/// The provider kind a host-process scope's lease records: its workspace
/// is a directory under the run dir, governed by the run's retention.
pub const HOST_PROVIDER: &str = "host";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxResourceRecord {
    pub lease:       SandboxLeaseId,
    pub allocation:  SandboxAllocationKey,
    /// The provider kind: [`HOST_PROVIDER`], or the sandbox plugin kind the
    /// lease manager recorded at allocation.
    pub provider:    SmolStr,
    /// The provider's own id for the resource, once it exists.
    #[serde(default)]
    pub resource_id: Option<SmolStr>,
    pub workspace:   WorkspaceId,
    #[serde(default)]
    pub state:       LeaseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending:     Option<PendingIntent>,
    /// The non-secret fingerprint of the backend the resource lives on: the
    /// daemon or endpoint, and the account or target. Never credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<SmolStr>,
}

impl SandboxResourceRecord {
    /// Whether this lease still needs release. Host leases can own action
    /// hosts. A stopped sandbox with no pending intent already had its
    /// retention applied by its owning invocation.
    pub fn needs_release(&self) -> bool {
        self.state != LeaseState::Deleted
            && (self.provider == HOST_PROVIDER
                || self.pending.is_some()
                || self.state == LeaseState::Live
                || (self.state == LeaseState::Allocating && self.fingerprint.is_some()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error("could not {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`{path}` is not a sandbox resource record: {source}")]
    Decode {
        path:   PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not encode a sandbox resource record: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("sandbox lease {0} is recorded more than once")]
    DuplicateLease(SandboxLeaseId),
    #[error("sandbox allocation {0:?} is recorded more than once")]
    DuplicateAllocation(SandboxAllocationKey),
    #[error("sandbox resource file `{path}` does not match lease {lease}")]
    AddressMismatch {
        path:  PathBuf,
        lease: SandboxLeaseId,
    },
    #[error("sandbox allocation {0:?} does not match its recorded resource")]
    AllocationMismatch(SandboxAllocationKey),
    #[error("unknown sandbox lease {0}")]
    UnknownLease(SandboxLeaseId),
    #[error("sandbox lease {0} was deleted; its workspace is gone")]
    DeletedLease(SandboxLeaseId),
}

/// Provider-neutral durable sandbox leases under one run's `resources/`.
pub struct ResourceStore {
    root:          PathBuf,
    by_lease:      BTreeMap<SandboxLeaseId, SandboxResourceRecord>,
    by_allocation: BTreeMap<SandboxAllocationKey, SandboxLeaseId>,
    next_lease:    u64,
}

impl ResourceStore {
    pub fn load(root: impl Into<PathBuf>) -> Result<Self, ResourceError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| resource_io("create", &root, source))?;
        let mut by_lease = BTreeMap::new();
        let mut by_allocation = BTreeMap::new();
        let mut next_lease = 0_u64;
        for entry in fs::read_dir(&root).map_err(|source| resource_io("read", &root, source))? {
            let entry = entry.map_err(|source| resource_io("read", &root, source))?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path).map_err(|source| resource_io("read", &path, source))?;
            let record: SandboxResourceRecord =
                serde_json::from_slice(&bytes).map_err(|source| ResourceError::Decode {
                    path: path.clone(),
                    source,
                })?;
            let expected_name = format!("{:016x}.json", record.lease.raw());
            if path.file_name().and_then(|name| name.to_str()) != Some(&expected_name) {
                return Err(ResourceError::AddressMismatch {
                    path,
                    lease: record.lease,
                });
            }
            if by_lease.insert(record.lease, record.clone()).is_some() {
                return Err(ResourceError::DuplicateLease(record.lease));
            }
            if by_allocation
                .insert(record.allocation, record.lease)
                .is_some()
            {
                return Err(ResourceError::DuplicateAllocation(record.allocation));
            }
            next_lease = next_lease.max(record.lease.raw().saturating_add(1));
        }
        Ok(Self {
            root,
            by_lease,
            by_allocation,
            next_lease,
        })
    }

    pub fn records(&self) -> impl Iterator<Item = &SandboxResourceRecord> {
        self.by_lease.values()
    }

    /// The record of `lease`, tombstones included: replay of a historical
    /// inherited invocation resolves through here.
    pub fn resolve(&self, lease: SandboxLeaseId) -> Result<&SandboxResourceRecord, ResourceError> {
        self.by_lease
            .get(&lease)
            .ok_or(ResourceError::UnknownLease(lease))
    }

    /// The record of `lease` for new work: a tombstone is refused, because
    /// the workspace it named is gone.
    pub fn resolve_usable(
        &self,
        lease: SandboxLeaseId,
    ) -> Result<&SandboxResourceRecord, ResourceError> {
        let record = self.resolve(lease)?;
        if record.state == LeaseState::Deleted {
            return Err(ResourceError::DeletedLease(lease));
        }
        Ok(record)
    }

    /// The lease of `allocation`, reserved on first use. `provider` is the
    /// kind the scope's runtime target names; the sandbox lease manager
    /// records the real one, with its fingerprint, when it allocates. A host
    /// lease is live from the start: its workspace is a directory.
    pub fn ensure_record(
        &mut self,
        allocation: SandboxAllocationKey,
        provider: impl Into<SmolStr>,
        workspace: WorkspaceId,
    ) -> Result<&SandboxResourceRecord, ResourceError> {
        let provider = provider.into();
        if let Some(lease) = self.by_allocation.get(&allocation).copied() {
            let record = self
                .by_lease
                .get(&lease)
                .expect("both resource indexes are written together");
            if record.workspace != workspace {
                return Err(ResourceError::AllocationMismatch(allocation));
            }
            return Ok(record);
        }
        let lease = SandboxLeaseId::new(self.next_lease);
        let host = provider == HOST_PROVIDER;
        let record = SandboxResourceRecord {
            lease,
            allocation,
            provider,
            resource_id: host.then(|| SmolStr::new(workspace.as_str())),
            workspace,
            state: if host {
                LeaseState::Live
            } else {
                LeaseState::Allocating
            },
            pending: None,
            fingerprint: None,
        };
        self.write(&record)?;
        self.next_lease = self.next_lease.saturating_add(1);
        self.by_allocation.insert(allocation, lease);
        self.by_lease.insert(lease, record);
        Ok(self
            .by_lease
            .get(&lease)
            .expect("the new resource was inserted"))
    }

    /// Change one record in place, durably: written before the index is
    /// updated, so a crash leaves the file and the memory in agreement.
    pub fn update(
        &mut self,
        lease: SandboxLeaseId,
        update: impl FnOnce(&mut SandboxResourceRecord),
    ) -> Result<(), ResourceError> {
        let mut record = self.resolve(lease)?.clone();
        update(&mut record);
        self.write(&record)?;
        self.by_lease.insert(lease, record);
        Ok(())
    }

    fn write(&self, record: &SandboxResourceRecord) -> Result<(), ResourceError> {
        let path = self.root.join(format!("{:016x}.json", record.lease.raw()));
        let bytes = serde_json::to_vec_pretty(record).map_err(ResourceError::Encode)?;
        write_atomically_with(&path, &bytes, resource_io)
    }
}

fn resource_io(action: &'static str, path: &Path, source: io::Error) -> ResourceError {
    ResourceError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

/// The resource store as the sandbox lease manager's ledger. The
/// coordinator reserves every lease first; an unknown lease here is a
/// caller that acquired without one, and is refused rather than invented.
#[derive(Clone)]
pub struct ResourceLedger(Arc<Mutex<ResourceStore>>);

impl ResourceLedger {
    pub fn new(store: Arc<Mutex<ResourceStore>>) -> Self {
        Self(store)
    }

    fn update(
        &self,
        lease: SandboxLeaseId,
        update: impl FnOnce(&mut SandboxResourceRecord),
    ) -> Result<(), LedgerError> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .update(lease, update)
            .map_err(|error| LedgerError(error.to_string()))
    }
}

impl LeaseLedger for ResourceLedger {
    fn lookup(&self, lease: SandboxLeaseId) -> Result<Option<LeaseRecord>, LedgerError> {
        let store = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Ok(record) = store.resolve(lease) else {
            return Ok(None);
        };
        Ok(Some(LeaseRecord {
            state:       record.state,
            pending:     record.pending,
            provider:    Some(record.provider.clone()),
            resource_id: record.resource_id.clone(),
            fingerprint: record.fingerprint.clone(),
        }))
    }

    fn allocating(
        &self,
        lease: SandboxLeaseId,
        provider: &str,
        fingerprint: &str,
    ) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Allocating;
            record.pending = None;
            record.provider = SmolStr::new(provider);
            record.fingerprint = Some(SmolStr::new(fingerprint));
        })
    }

    fn live(&self, lease: SandboxLeaseId, resource_id: &str) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Live;
            record.pending = None;
            record.resource_id = Some(SmolStr::new(resource_id));
        })
    }

    fn pending(&self, lease: SandboxLeaseId, intent: PendingIntent) -> Result<(), LedgerError> {
        self.update(lease, |record| record.pending = Some(intent))
    }

    fn stopped(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Stopped;
            record.pending = None;
        })
    }

    fn deleted(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Deleted;
            record.pending = None;
        })
    }
}
