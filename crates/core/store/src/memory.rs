//! The in-memory backend: for tests, and for a host that keeps no run of
//! record. No file is created; the lease lives in memory.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use crate::{Access, Digest, LogId, OwnerId, Record, RunKey, RunLogs, RunStore, StoreError};

/// One run's logs, blobs and lease.
#[derive(Default)]
struct MemoryRun {
    logs:  Mutex<BTreeMap<LogId, Vec<Record>>>,
    blobs: Mutex<BTreeMap<Digest, Vec<u8>>>,
    /// The owner holding the writer lease, and its live handle so a retry by
    /// the same owner shares it.
    lease: Mutex<Option<(OwnerId, Weak<MemoryLogs>)>>,
}

/// A store of runs held in memory.
#[derive(Default)]
pub struct MemoryRunStore {
    runs: Mutex<BTreeMap<RunKey, Arc<MemoryRun>>>,
}

impl MemoryRunStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Release the writer lease of `key` as an operator would: the current
    /// owner's handle turns stale, and the next `Write` open takes the run.
    pub fn release(&self, key: &RunKey) {
        if let Some(run) = lock(&self.runs).get(key) {
            *lock(&run.lease) = None;
        }
    }

    /// The owner holding the writer lease of `key`, if any.
    pub fn owner(&self, key: &RunKey) -> Option<OwnerId> {
        lock(&self.runs)
            .get(key)
            .and_then(|run| lock(&run.lease).as_ref().map(|(owner, _)| owner.clone()))
    }

    fn locator(key: &RunKey) -> String {
        format!("memory run `{key}`")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[async_trait::async_trait]
impl RunStore for MemoryRunStore {
    async fn open(&self, key: &RunKey, access: Access) -> Result<Arc<dyn RunLogs>, StoreError> {
        let mut runs = lock(&self.runs);
        let run = match &access {
            Access::Create { .. } => {
                if runs.contains_key(key) {
                    return Err(StoreError::Exists {
                        key:     key.clone(),
                        locator: Self::locator(key),
                    });
                }
                runs.entry(key.clone()).or_default().clone()
            }
            Access::Write { .. } | Access::Read => {
                runs.get(key).cloned().ok_or_else(|| StoreError::NotFound {
                    key:     key.clone(),
                    locator: Self::locator(key),
                })?
            }
        };
        drop(runs);
        let Some(owner) = access.owner() else {
            return Ok(Arc::new(MemoryLogs {
                key:   key.clone(),
                run:   run.clone(),
                owner: None,
            }));
        };
        let mut lease = lock(&run.lease);
        if let Some((holder, handle)) = lease.as_ref()
            && let Some(handle) = handle.upgrade()
        {
            if holder == owner {
                return Ok(handle);
            }
            return Err(StoreError::Leased {
                locator: Self::locator(key),
                owner:   holder.clone(),
            });
        }
        let handle = Arc::new(MemoryLogs {
            key:   key.clone(),
            run:   run.clone(),
            owner: Some(owner.clone()),
        });
        *lease = Some((owner.clone(), Arc::downgrade(&handle)));
        Ok(handle)
    }
}

/// One run in memory, opened.
struct MemoryLogs {
    key:   RunKey,
    run:   Arc<MemoryRun>,
    owner: Option<OwnerId>,
}

impl MemoryLogs {
    /// Whether this handle may write: it was opened with an owner that
    /// still holds the lease.
    fn check_writer(&self) -> Result<(), StoreError> {
        let owner = self.owner.as_ref().ok_or(StoreError::ReadOnly)?;
        let lease = lock(&self.run.lease);
        match lease.as_ref() {
            Some((holder, _)) if holder == owner => Ok(()),
            _ => Err(StoreError::StaleOwner),
        }
    }
}

impl Drop for MemoryLogs {
    fn drop(&mut self) {
        let Some(owner) = &self.owner else {
            return;
        };
        let mut lease = lock(&self.run.lease);
        if lease.as_ref().is_some_and(|(holder, _)| holder == owner) {
            *lease = None;
        }
    }
}

#[async_trait::async_trait]
impl RunLogs for MemoryLogs {
    fn locator(&self) -> String {
        MemoryRunStore::locator(&self.key)
    }

    async fn append(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError> {
        self.check_writer()?;
        let mut logs = lock(&self.run.logs);
        let stored = logs.entry(*log).or_default();
        let head = stored.len() as u64;
        let fresh = crate::admit(log, head, records, |seq| {
            usize::try_from(seq)
                .ok()
                .and_then(|seq| stored.get(seq).cloned())
        })?;
        stored.extend(fresh.into_iter().cloned());
        Ok(())
    }

    async fn read(&self, log: &LogId) -> Result<Vec<Record>, StoreError> {
        Ok(lock(&self.run.logs).get(log).cloned().unwrap_or_default())
    }

    async fn put_blob(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        self.check_writer()?;
        let digest = Digest::of(bytes);
        lock(&self.run.blobs)
            .entry(digest)
            .or_insert_with(|| bytes.to_vec());
        Ok(digest)
    }

    async fn get_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(lock(&self.run.blobs).get(&digest).cloned())
    }
}
