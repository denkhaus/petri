use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use executor::WorkspaceId;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::{SandboxAllocationKey, SandboxLeaseId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxResourceRecord {
    pub lease:       SandboxLeaseId,
    pub allocation:  SandboxAllocationKey,
    pub provider:    SmolStr,
    pub resource_id: SmolStr,
    pub workspace:   WorkspaceId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSandbox {
    pub resource_id: SmolStr,
    pub workspace:   WorkspaceId,
}

#[async_trait::async_trait]
pub trait SandboxAdapter: Send + Sync {
    fn provider(&self) -> &str;

    async fn ensure(
        &self,
        allocation: SandboxAllocationKey,
    ) -> Result<ProviderSandbox, ResourceError>;

    async fn attach(
        &self,
        record: &SandboxResourceRecord,
    ) -> Result<ProviderSandbox, ResourceError>;

    async fn release(
        &self,
        record: &SandboxResourceRecord,
        outcome: executor::ScopeOutcome,
    ) -> Result<(), ResourceError>;
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
    #[error("sandbox resource provider mismatch: recorded `{recorded}`, configured `{configured}`")]
    ProviderMismatch {
        recorded:   SmolStr,
        configured: SmolStr,
    },
    #[error("sandbox provider failed: {0}")]
    Provider(SmolStr),
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

    pub fn resolve(&self, lease: SandboxLeaseId) -> Result<&SandboxResourceRecord, ResourceError> {
        self.by_lease
            .get(&lease)
            .ok_or(ResourceError::UnknownLease(lease))
    }

    pub fn for_allocation(
        &self,
        allocation: SandboxAllocationKey,
    ) -> Option<&SandboxResourceRecord> {
        self.by_allocation
            .get(&allocation)
            .and_then(|lease| self.by_lease.get(lease))
    }

    pub async fn ensure(
        &mut self,
        allocation: SandboxAllocationKey,
        adapter: &dyn SandboxAdapter,
    ) -> Result<&SandboxResourceRecord, ResourceError> {
        if let Some(lease) = self.by_allocation.get(&allocation).copied() {
            let record = self
                .by_lease
                .get(&lease)
                .expect("both resource indexes are written together");
            if record.provider != adapter.provider() {
                return Err(ResourceError::ProviderMismatch {
                    recorded:   record.provider.clone(),
                    configured: SmolStr::new(adapter.provider()),
                });
            }
            adapter.attach(record).await?;
            return Ok(self
                .by_lease
                .get(&lease)
                .expect("the attached resource remains indexed"));
        }

        let sandbox = adapter.ensure(allocation).await?;
        let lease = SandboxLeaseId::new(self.next_lease);
        let record = SandboxResourceRecord {
            lease,
            allocation,
            provider: SmolStr::new(adapter.provider()),
            resource_id: sandbox.resource_id,
            workspace: sandbox.workspace,
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

    pub fn ensure_record(
        &mut self,
        allocation: SandboxAllocationKey,
        provider: impl Into<SmolStr>,
        resource_id: impl Into<SmolStr>,
        workspace: WorkspaceId,
    ) -> Result<&SandboxResourceRecord, ResourceError> {
        let provider = provider.into();
        let resource_id = resource_id.into();
        if let Some(lease) = self.by_allocation.get(&allocation).copied() {
            let record = self
                .by_lease
                .get(&lease)
                .expect("both resource indexes are written together");
            if record.provider != provider
                || record.resource_id != resource_id
                || record.workspace != workspace
            {
                return Err(ResourceError::AllocationMismatch(allocation));
            }
            return Ok(record);
        }
        let lease = SandboxLeaseId::new(self.next_lease);
        let record = SandboxResourceRecord {
            lease,
            allocation,
            provider,
            resource_id,
            workspace,
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

    fn write(&self, record: &SandboxResourceRecord) -> Result<(), ResourceError> {
        let path = self.root.join(format!("{:016x}.json", record.lease.raw()));
        let temporary = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(record).map_err(ResourceError::Encode)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|source| resource_io("create", &temporary, source))?;
        file.write_all(&bytes)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_data())
            .map_err(|source| resource_io("write", &temporary, source))?;
        fs::rename(&temporary, &path).map_err(|source| resource_io("rename", &path, source))?;
        File::open(&self.root)
            .and_then(|directory| directory.sync_data())
            .map_err(|source| resource_io("sync", &self.root, source))
    }
}

fn resource_io(action: &'static str, path: &Path, source: io::Error) -> ResourceError {
    ResourceError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}
