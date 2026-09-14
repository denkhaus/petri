//! The run-directory backend: one directory per run, one JSONL file per
//! log, one file per blob, and the run's lease as a file lock.
//!
//! Layout under the run directory:
//!
//! - `run.json`: the store's own index, `{"key": <run key>}`, and the file the
//!   writer lease locks.
//! - `coordinator.jsonl`: the coordinator log, one record per line.
//! - `resources.jsonl`: the sandbox resource log, one record per line.
//! - `graphs/<digest>.json`: every blob, byte-exact, named by its digest.
//! - `executions/<execution>/events.jsonl`: one engine log per execution.
//!
//! A line is the record's JSON value, so the coordinator and engine logs
//! hold exactly the lines a public event carries under `record`. A torn
//! final line (EOF before its newline) is dropped: a writer truncates it
//! before it appends, a reader leaves the file alone.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::{JoinError, spawn_blocking};

use crate::jsonl::clean_lines;
use crate::{
    Access, Digest, ExecutionId, LogId, OwnerId, Record, RunKey, RunLogs, RunStore, StoreError,
};

pub const RUN_FILE: &str = "run.json";
pub const COORDINATOR_FILE: &str = "coordinator.jsonl";
pub const RESOURCES_FILE: &str = "resources.jsonl";
pub const GRAPHS_DIR: &str = "graphs";
pub const EXECUTIONS_DIR: &str = "executions";
pub const EVENTS_FILE: &str = "events.jsonl";

/// An execution's directory relative to the run directory: the one spelling
/// of `executions/<execution>` the store and the host's execution work
/// directory share.
pub fn execution_relative_dir(execution: ExecutionId) -> PathBuf {
    Path::new(EXECUTIONS_DIR).join(format!("{:016x}", execution.raw()))
}

/// The store's own index under the run directory.
#[derive(Serialize, Deserialize)]
struct RunFile {
    key: RunKey,
}

/// A store of one run, the one under `run_dir`.
pub struct RunDirStore {
    root:  PathBuf,
    /// The live writer handle, so a retry by its owner shares the lease and
    /// another owner in this process is refused without touching the lock.
    lease: Mutex<Option<(OwnerId, Weak<RunDirLogs>)>>,
}

impl RunDirStore {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            root:  run_dir.into(),
            lease: Mutex::new(None),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The key of the run stored here, read from `run.json`; `None` when
    /// the directory holds no run.
    pub fn stored_key(&self) -> Result<Option<RunKey>, StoreError> {
        let path = self.root.join(RUN_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(self.io("read run.json", source)),
        };
        let run: RunFile = serde_json::from_slice(&bytes)
            .map_err(|source| StoreError::backend(self.locator(), "read run.json", source))?;
        Ok(Some(run.key))
    }

    fn locator(&self) -> String {
        self.root.display().to_string()
    }

    fn io(&self, action: &'static str, source: io::Error) -> StoreError {
        StoreError::io(self.locator(), action, source)
    }

    fn check_key(&self, key: &RunKey) -> Result<(), StoreError> {
        match self.stored_key()? {
            Some(stored) if stored == *key => Ok(()),
            _ => Err(StoreError::NotFound {
                key:     key.clone(),
                locator: self.locator(),
            }),
        }
    }

    fn acquire_lease(&self, file: &File, owner: &OwnerId) -> Result<(), StoreError> {
        file.try_lock().map_err(|source| match source {
            fs::TryLockError::WouldBlock => StoreError::Leased {
                locator: self.locator(),
                // The holder is another process; its owner id is not
                // readable through the lock.
                owner:   owner.clone(),
            },
            fs::TryLockError::Error(source) => self.io("lock run.json", source),
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[async_trait::async_trait]
impl RunStore for RunDirStore {
    async fn open(&self, key: &RunKey, access: Access) -> Result<Arc<dyn RunLogs>, StoreError> {
        let Some(owner) = access.owner() else {
            self.check_key(key)?;
            return Ok(Arc::new(RunDirLogs {
                key:   key.clone(),
                owner: None,
                state: Arc::new(WriterState::new(self.root.clone())),
                _lock: None,
            }));
        };
        let mut lease = lock(&self.lease);
        if let Some((holder, handle)) = lease.as_ref()
            && let Some(handle) = handle.upgrade()
        {
            if holder == owner
                && access
                    == (Access::Write {
                        owner: owner.clone(),
                    })
            {
                return Ok(handle);
            }
            return Err(StoreError::Leased {
                locator: self.locator(),
                owner:   holder.clone(),
            });
        }
        let path = self.root.join(RUN_FILE);
        let lock_file = match &access {
            Access::Create { .. } => {
                fs::create_dir_all(&self.root).map_err(|source| self.io("create", source))?;
                fs::create_dir_all(self.root.join(GRAPHS_DIR))
                    .map_err(|source| self.io("create graphs/", source))?;
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|source| {
                        if source.kind() == io::ErrorKind::AlreadyExists {
                            StoreError::Exists {
                                key:     key.clone(),
                                locator: self.locator(),
                            }
                        } else {
                            self.io("create run.json", source)
                        }
                    })?;
                self.acquire_lease(&file, owner)?;
                let bytes = serde_json::to_vec_pretty(&RunFile { key: key.clone() })
                    .map_err(|source| StoreError::backend(self.locator(), "encode", source))?;
                file.write_all(&bytes)
                    .and_then(|()| file.flush())
                    .and_then(|()| file.sync_data())
                    .map_err(|source| self.io("write run.json", source))?;
                file
            }
            Access::Write { .. } => {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|source| {
                        if source.kind() == io::ErrorKind::NotFound {
                            StoreError::NotFound {
                                key:     key.clone(),
                                locator: self.locator(),
                            }
                        } else {
                            self.io("open run.json", source)
                        }
                    })?;
                self.acquire_lease(&file, owner)?;
                self.check_key(key)?;
                file
            }
            Access::Read => unreachable!("a read open names no owner"),
        };
        let handle = Arc::new(RunDirLogs {
            key:   key.clone(),
            owner: Some(owner.clone()),
            state: Arc::new(WriterState::new(self.root.clone())),
            _lock: Some(lock_file),
        });
        *lease = Some((owner.clone(), Arc::downgrade(&handle)));
        Ok(handle)
    }
}

/// One log's open append handle and head.
struct LogHead {
    file: File,
    /// The seq the next record takes: the count of complete lines.
    next: u64,
}

/// What the file operations need: the root, and each opened log's head.
struct WriterState {
    root:  PathBuf,
    heads: Mutex<BTreeMap<LogId, Arc<Mutex<LogHead>>>>,
}

impl WriterState {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            heads: Mutex::new(BTreeMap::new()),
        }
    }

    fn locator(&self) -> String {
        self.root.display().to_string()
    }

    fn io(&self, action: &'static str, source: io::Error) -> StoreError {
        StoreError::io(self.locator(), action, source)
    }

    fn log_path(&self, log: &LogId) -> PathBuf {
        match log {
            LogId::Coordinator => self.root.join(COORDINATOR_FILE),
            LogId::Resources => self.root.join(RESOURCES_FILE),
            LogId::Execution(execution) => self
                .root
                .join(execution_relative_dir(*execution))
                .join(EVENTS_FILE),
        }
    }

    fn blob_path(&self, digest: Digest) -> PathBuf {
        self.root.join(GRAPHS_DIR).join(format!("{digest}.json"))
    }

    /// The log's head, opening the file on first use: a torn tail is
    /// truncated, the complete lines counted, and the file opened to append.
    fn head(&self, log: &LogId) -> Result<Arc<Mutex<LogHead>>, StoreError> {
        let mut heads = lock(&self.heads);
        if let Some(head) = heads.get(log) {
            return Ok(head.clone());
        }
        let path = self.log_path(log);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| self.io("create", source))?;
        }
        let next = match fs::read(&path) {
            Ok(bytes) => {
                let lines = clean_lines(&bytes);
                let (clean_len, torn) = (lines.clean_len, lines.torn);
                let count = lines.count() as u64;
                if torn {
                    let file = OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .map_err(|source| self.io("open", source))?;
                    file.set_len(clean_len as u64)
                        .and_then(|()| file.sync_data())
                        .map_err(|source| self.io("truncate", source))?;
                }
                count
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => 0,
            Err(source) => return Err(self.io("read", source)),
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| self.io("open", source))?;
        let head = Arc::new(Mutex::new(LogHead { file, next }));
        heads.insert(*log, head.clone());
        Ok(head)
    }

    fn append(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError> {
        let head = self.head(log)?;
        let mut head = lock(&head);
        let stored = |seq: u64| self.read_log(log).ok()?.into_iter().find(|r| r.seq == seq);
        let fresh = crate::admit(log, head.next, records, stored)?;
        if fresh.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        for record in &fresh {
            serde_json::to_writer(&mut bytes, &record.record)
                .map_err(|source| StoreError::backend(self.locator(), "encode", source))?;
            bytes.push(b'\n');
        }
        head.file
            .write_all(&bytes)
            .and_then(|()| head.file.flush())
            .and_then(|()| head.file.sync_data())
            .map_err(|source| self.io("append", source))?;
        head.next += fresh.len() as u64;
        Ok(())
    }

    fn read_log(&self, log: &LogId) -> Result<Vec<Record>, StoreError> {
        let path = self.log_path(log);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(self.io("read", source)),
        };
        let mut records = Vec::new();
        for line in clean_lines(&bytes) {
            let value: Value = serde_json::from_slice(line).map_err(|source| {
                StoreError::backend(
                    path.display().to_string(),
                    "read",
                    format!("line {} is not JSON: {source}", records.len() + 1),
                )
            })?;
            let record = Record::from_value(value).map_err(|source| {
                StoreError::backend(path.display().to_string(), "read", source)
            })?;
            records.push(record);
        }
        Ok(records)
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        let digest = Digest::of(bytes);
        let path = self.blob_path(digest);
        if path.exists() {
            // Content-addressed: the same bytes are already there.
            return Ok(digest);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| self.io("create", source))?;
        }
        write_atomically(&path, bytes).map_err(|(action, source)| self.io(action, source))?;
        Ok(digest)
    }

    fn get_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, StoreError> {
        match fs::read(self.blob_path(digest)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(self.io("read", source)),
        }
    }
}

/// Publish `bytes` at `path`: a temp write with fsync, a rename, and a
/// parent-directory sync, so a crash leaves the file whole or absent.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), (&'static str, io::Error)> {
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|source| ("create", source))?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_data())
        .map_err(|source| ("write", source))?;
    fs::rename(&temporary, path).map_err(|source| ("rename", source))?;
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_data())
            .map_err(|source| ("sync", source))?;
    }
    Ok(())
}

/// One run directory, opened.
pub struct RunDirLogs {
    key:   RunKey,
    owner: Option<OwnerId>,
    state: Arc<WriterState>,
    /// The lease: `run.json`, locked for the handle's life.
    _lock: Option<File>,
}

impl RunDirLogs {
    /// The run's key.
    pub fn key(&self) -> &RunKey {
        &self.key
    }

    /// The run directory.
    pub fn root(&self) -> &Path {
        &self.state.root
    }

    fn check_writer(&self) -> Result<(), StoreError> {
        // The file lock ends with the process, so a handle that holds it is
        // never stale: the only refusal is a reader asked to write.
        self.owner.as_ref().map(|_| ()).ok_or(StoreError::ReadOnly)
    }

    fn joined<T>(&self, result: Result<T, JoinError>) -> Result<T, StoreError> {
        result.map_err(|source| StoreError::backend(self.locator(), "run", source))
    }
}

#[async_trait::async_trait]
impl RunLogs for RunDirLogs {
    fn locator(&self) -> String {
        self.state.locator()
    }

    async fn append(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError> {
        self.check_writer()?;
        let state = self.state.clone();
        let log = *log;
        let records = records.to_vec();
        self.joined(spawn_blocking(move || state.append(&log, &records)).await)?
    }

    async fn read(&self, log: &LogId) -> Result<Vec<Record>, StoreError> {
        let state = self.state.clone();
        let log = *log;
        self.joined(spawn_blocking(move || state.read_log(&log)).await)?
    }

    async fn put_blob(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        self.check_writer()?;
        let state = self.state.clone();
        let bytes = bytes.to_vec();
        self.joined(spawn_blocking(move || state.put_blob(&bytes)).await)?
    }

    async fn get_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, StoreError> {
        let state = self.state.clone();
        self.joined(spawn_blocking(move || state.get_blob(digest)).await)?
    }
}
