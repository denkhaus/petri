use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ir::Graph;
use serde::{Deserialize, Serialize};

use crate::jsonl::clean_lines;
use crate::{
    COORDINATOR_FORMAT_VERSION, CoordinatorEvent, CoordinatorRecord, CoordinatorState, ExecutionId,
    GraphDigest, InvocationId, StateError,
};

pub const RUN_FILE: &str = "run.json";
pub const COORDINATOR_FILE: &str = "coordinator.jsonl";
pub const GRAPHS_DIR: &str = "graphs";
pub const RESOURCES_DIR: &str = "resources";
pub const INVOCATIONS_DIR: &str = "invocations";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMetadata {
    pub format_version: u32,
    pub root:           InvocationId,
}

#[derive(Debug)]
pub struct DecodedCoordinatorLog {
    pub records:   Vec<CoordinatorRecord>,
    pub clean_len: usize,
    pub torn:      bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("run directory `{0}` is already in use")]
    Leased(PathBuf),
    #[error("`{path}` contains an invalid coordinator record on line {line}")]
    BadRecord {
        path:   PathBuf,
        line:   usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("`{path}` contains invalid JSON: {source}")]
    BadJson {
        path:   PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("run format {found} is unsupported; expected {expected}")]
    UnsupportedFormat { found: u32, expected: u32 },
    #[error(transparent)]
    State(#[from] StateError),
    #[error("could not encode durable state: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("registered graph {0} is missing")]
    MissingGraph(GraphDigest),
    #[error("registered graph {expected} hashes to {found}")]
    GraphDigest {
        expected: GraphDigest,
        found:    GraphDigest,
    },
    #[error("registered graph {digest} is not a graph: {source}")]
    BadGraph {
        digest: GraphDigest,
        #[source]
        source: serde_json::Error,
    },
    #[error("registered graph {digest} failed validation: {message}")]
    InvalidGraph {
        digest:  GraphDigest,
        message: String,
    },
}

/// A leased, durable coordinator log and graph registry.
pub struct CoordinatorStore {
    root:     PathBuf,
    _lease:   File,
    log:      File,
    state:    CoordinatorState,
    next_seq: u64,
    /// Decoded and validated graphs by digest, so repeated loads — restarts of
    /// one invocation, resume-time resource checks — parse and validate once.
    graphs:   BTreeMap<GraphDigest, Arc<Graph>>,
    /// The records `create` appended before an observer could attach.
    opening:  Vec<CoordinatorRecord>,
}

impl CoordinatorStore {
    pub fn create(
        root: impl Into<PathBuf>,
        middleware_chain: Vec<engine::MiddlewareKey>,
    ) -> Result<Self, StoreError> {
        let root = root.into();
        create_dir(&root)?;
        create_dir(&root.join(GRAPHS_DIR))?;
        create_dir(&root.join(RESOURCES_DIR))?;
        create_dir(&root.join(INVOCATIONS_DIR))?;

        let metadata_path = root.join(RUN_FILE);
        let mut lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&metadata_path)
            .map_err(|source| io_error("create", &metadata_path, source))?;
        acquire_lease(&lease, &root)?;
        let metadata = RunMetadata {
            format_version: COORDINATOR_FORMAT_VERSION,
            root:           InvocationId::ROOT,
        };
        write_json(&mut lease, &metadata, &metadata_path)?;

        let log_path = root.join(COORDINATOR_FILE);
        let log = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&log_path)
            .map_err(|source| io_error("create", &log_path, source))?;
        let mut store = Self {
            root,
            _lease: lease,
            log,
            state: CoordinatorState::default(),
            next_seq: 0,
            graphs: BTreeMap::new(),
            opening: Vec::new(),
        };
        let started = store.append(CoordinatorEvent::RunStarted {
            format_version: COORDINATOR_FORMAT_VERSION,
            root: InvocationId::ROOT,
            middleware_chain,
        })?;
        store.opening.push(started);
        Ok(store)
    }

    pub fn resume(root: impl Into<PathBuf>) -> Result<(Self, bool), StoreError> {
        let root = root.into();
        let metadata_path = root.join(RUN_FILE);
        let lease = hold_run_lease(&root)?;
        let metadata: RunMetadata = read_json(&metadata_path)?;
        if metadata.format_version != COORDINATOR_FORMAT_VERSION {
            return Err(StoreError::UnsupportedFormat {
                found:    metadata.format_version,
                expected: COORDINATOR_FORMAT_VERSION,
            });
        }
        if metadata.root != InvocationId::ROOT {
            return Err(StateError::InvalidRootInvocation.into());
        }

        let log_path = root.join(COORDINATOR_FILE);
        let bytes = fs::read(&log_path).map_err(|source| io_error("read", &log_path, source))?;
        let decoded = decode_coordinator_log(&log_path, &bytes)?;
        let state = CoordinatorState::replay(&decoded.records)?;
        if decoded.torn {
            let file = OpenOptions::new()
                .write(true)
                .open(&log_path)
                .map_err(|source| io_error("open", &log_path, source))?;
            file.set_len(decoded.clean_len as u64)
                .map_err(|source| io_error("truncate", &log_path, source))?;
            file.sync_data()
                .map_err(|source| io_error("sync", &log_path, source))?;
        }
        let graphs = verify_graph_registry(&root, &state)?;
        let log = OpenOptions::new()
            .append(true)
            .open(&log_path)
            .map_err(|source| io_error("open", &log_path, source))?;
        let next_seq = decoded.records.len() as u64;
        Ok((
            Self {
                root,
                _lease: lease,
                log,
                state,
                next_seq,
                graphs,
                opening: Vec::new(),
            },
            decoded.torn,
        ))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state(&self) -> &CoordinatorState {
        &self.state
    }

    /// The records `create` appended before anyone could observe them: the
    /// run's own start. A resumed store opened with none.
    pub fn opening_records(&self) -> &[CoordinatorRecord] {
        &self.opening
    }

    pub fn append(&mut self, event: CoordinatorEvent) -> Result<CoordinatorRecord, StoreError> {
        let mut next_state = self.state.clone();
        next_state.apply(&event)?;
        let record = CoordinatorRecord {
            seq: self.next_seq,
            event,
        };
        let mut encoded = serde_json::to_vec(&record).map_err(StoreError::Encode)?;
        encoded.push(b'\n');
        let path = self.root.join(COORDINATOR_FILE);
        self.log
            .write_all(&encoded)
            .and_then(|()| self.log.flush())
            .and_then(|()| self.log.sync_data())
            .map_err(|source| io_error("append", &path, source))?;
        self.state = next_state;
        self.next_seq += 1;
        self.write_invocation_projection(&record.event)?;
        Ok(record)
    }

    /// Persist and register a graph. The record is the `GraphRegistered`
    /// append when the graph was new, `None` when it was already registered.
    pub fn register_graph(
        &mut self,
        graph: &Graph,
    ) -> Result<(GraphDigest, Option<CoordinatorRecord>), StoreError> {
        let bytes = ir::encode_graph(graph).map_err(StoreError::Encode)?;
        self.register_graph_bytes(&bytes)
    }

    pub(crate) fn register_graph_bytes(
        &mut self,
        bytes: &[u8],
    ) -> Result<(GraphDigest, Option<CoordinatorRecord>), StoreError> {
        let digest = digest_bytes(bytes);
        if self.state.graphs.contains(&digest) {
            let existing = self.graph_bytes(digest)?;
            if existing != bytes {
                return Err(StoreError::GraphDigest {
                    expected: digest,
                    found:    digest_bytes(&existing),
                });
            }
            return Ok((digest, None));
        }

        let path = self.graph_path(digest);
        write_once_atomically(&path, bytes)?;
        let record = self.append(CoordinatorEvent::GraphRegistered { digest })?;
        Ok((digest, Some(record)))
    }

    pub fn load_graph(&mut self, digest: GraphDigest) -> Result<Arc<Graph>, StoreError> {
        if !self.state.graphs.contains(&digest) {
            return Err(StoreError::MissingGraph(digest));
        }
        if let Some(graph) = self.graphs.get(&digest) {
            return Ok(graph.clone());
        }
        let graph = Arc::new(decode_graph(digest, &self.graph_bytes(digest)?)?);
        self.graphs.insert(digest, graph.clone());
        Ok(graph)
    }

    pub fn graph_bytes(&self, digest: GraphDigest) -> Result<Vec<u8>, StoreError> {
        let path = self.graph_path(digest);
        fs::read(&path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                StoreError::MissingGraph(digest)
            } else {
                io_error("read", &path, source)
            }
        })
    }

    pub fn graph_path(&self, digest: GraphDigest) -> PathBuf {
        self.root.join(GRAPHS_DIR).join(format!("{digest}.json"))
    }

    pub fn invocation_dir(&self, invocation: InvocationId) -> PathBuf {
        self.root
            .join(INVOCATIONS_DIR)
            .join(format!("{:016x}", invocation.raw()))
    }

    pub fn execution_dir(&self, invocation: InvocationId, execution: ExecutionId) -> PathBuf {
        self.root
            .join(execution_relative_dir(invocation, execution))
    }

    pub fn create_execution_dir(
        &self,
        invocation: InvocationId,
        execution: ExecutionId,
    ) -> Result<PathBuf, StoreError> {
        let path = self.execution_dir(invocation, execution);
        create_dir(&path)?;
        Ok(path)
    }

    fn write_invocation_projection(&self, event: &CoordinatorEvent) -> Result<(), StoreError> {
        let invocation = match event {
            CoordinatorEvent::InvocationDeclared { invocation, .. }
            | CoordinatorEvent::InvocationFinished { invocation, .. }
            | CoordinatorEvent::InvocationCancelRequested { invocation, .. }
            | CoordinatorEvent::ExecutionDeclared { invocation, .. } => Some(*invocation),
            CoordinatorEvent::ExecutionFinished { execution, .. } => self
                .state
                .executions
                .get(execution)
                .map(|state| state.declaration.invocation),
            _ => None,
        };
        let Some(invocation) = invocation else {
            return Ok(());
        };
        let Some(state) = self.state.invocations.get(&invocation) else {
            return Ok(());
        };
        let directory = self.invocation_dir(invocation);
        create_dir(&directory.join("executions"))?;
        let path = directory.join("invocation.json");
        let bytes = serde_json::to_vec_pretty(state).map_err(StoreError::Encode)?;
        // A rebuildable mirror of the log — published atomically for readers,
        // but not fsynced: the coordinator log is the durable source of truth.
        write_atomically_relaxed(&path, &bytes)
    }
}

pub fn decode_coordinator_log(
    path: &Path,
    bytes: &[u8],
) -> Result<DecodedCoordinatorLog, StoreError> {
    let lines = clean_lines(bytes);
    let (clean_len, torn) = (lines.clean_len, lines.torn);
    let mut records = Vec::new();
    for (index, line) in lines.enumerate() {
        let record = serde_json::from_slice(line).map_err(|source| StoreError::BadRecord {
            path: path.to_path_buf(),
            line: index + 1,
            source,
        })?;
        records.push(record);
    }
    Ok(DecodedCoordinatorLog {
        records,
        clean_len,
        torn,
    })
}

/// An execution's directory relative to the run directory: the one spelling
/// of `invocations/<invocation>/executions/<execution>` the store and its
/// read-only inspectors share.
pub(crate) fn execution_relative_dir(invocation: InvocationId, execution: ExecutionId) -> PathBuf {
    Path::new(INVOCATIONS_DIR)
        .join(format!("{:016x}", invocation.raw()))
        .join("executions")
        .join(format!("{:016x}", execution.raw()))
}

/// Verify every registered graph and keep the decoded results, seeding the
/// store's cache so the first `load_graph` does not repeat the work.
pub(crate) fn verify_graph_registry(
    root: &Path,
    state: &CoordinatorState,
) -> Result<BTreeMap<GraphDigest, Arc<Graph>>, StoreError> {
    let mut graphs = BTreeMap::new();
    for digest in &state.graphs {
        let path = root.join(GRAPHS_DIR).join(format!("{digest}.json"));
        let bytes = fs::read(&path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                StoreError::MissingGraph(*digest)
            } else {
                io_error("read", &path, source)
            }
        })?;
        // `decode_graph` digests, decodes, and validates in one pass.
        graphs.insert(*digest, Arc::new(decode_graph(*digest, &bytes)?));
    }
    Ok(graphs)
}

fn decode_graph(digest: GraphDigest, bytes: &[u8]) -> Result<Graph, StoreError> {
    let found = digest_bytes(bytes);
    if found != digest {
        return Err(StoreError::GraphDigest {
            expected: digest,
            found,
        });
    }
    let graph: Graph =
        serde_json::from_slice(bytes).map_err(|source| StoreError::BadGraph { digest, source })?;
    if let Err(errors) = ir::validate(&graph) {
        return Err(StoreError::InvalidGraph {
            digest,
            message: errors[0].to_string(),
        });
    }
    Ok(graph)
}

fn digest_bytes(bytes: &[u8]) -> GraphDigest {
    GraphDigest::from_bytes(ir::graph_digest_bytes(bytes))
}

/// Hold the run's lease without opening its log: what a maintenance command
/// (prune) takes so no coordinator can resume the run while it works. The
/// lock lives as long as the returned file. A run a live process holds is
/// [`StoreError::Leased`].
pub fn hold_run_lease(root: &Path) -> Result<File, StoreError> {
    let metadata_path = root.join(RUN_FILE);
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&metadata_path)
        .map_err(|source| io_error("open", &metadata_path, source))?;
    acquire_lease(&lease, root)?;
    Ok(lease)
}

fn acquire_lease(file: &File, root: &Path) -> Result<(), StoreError> {
    file.try_lock().map_err(|source| match source {
        fs::TryLockError::WouldBlock => StoreError::Leased(root.to_path_buf()),
        fs::TryLockError::Error(source) => io_error("lock", &root.join(RUN_FILE), source),
    })
}

fn create_dir(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path).map_err(|source| io_error("create", path, source))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, StoreError> {
    let bytes = fs::read(path).map_err(|source| io_error("read", path, source))?;
    serde_json::from_slice(&bytes).map_err(|source| StoreError::BadJson {
        path: path.to_path_buf(),
        source,
    })
}

fn write_json<T: Serialize>(file: &mut File, value: &T, path: &Path) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(StoreError::Encode)?;
    file.write_all(&bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_data())
        .map_err(|source| io_error("write", path, source))
}

fn write_once_atomically(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    if path.exists() {
        let existing = fs::read(path).map_err(|source| io_error("read", path, source))?;
        if existing == bytes {
            return Ok(());
        }
        return Err(StoreError::GraphDigest {
            expected: digest_bytes(bytes),
            found:    digest_bytes(&existing),
        });
    }
    write_atomically(path, bytes)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    write_atomically_with(path, bytes, io_error)
}

/// Publish `bytes` at `path` via temp write and rename, without any fsync.
/// Only for rebuildable mirrors whose loss a crash may ignore.
fn write_atomically_relaxed(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).map_err(|source| io_error("write", &temporary, source))?;
    fs::rename(&temporary, path).map_err(|source| io_error("rename", path, source))
}

/// Atomically publish `bytes` at `path` — temp write with fsync, rename, and a
/// parent-directory sync — mapping each failing IO action through `error`. The
/// one copy of the crash-safety plumbing every durable file in the crate uses.
pub(crate) fn write_atomically_with<E>(
    path: &Path,
    bytes: &[u8],
    error: impl Fn(&'static str, &Path, io::Error) -> E,
) -> Result<(), E> {
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|source| error("create", &temporary, source))?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_data())
        .map_err(|source| error("write", &temporary, source))?;
    fs::rename(&temporary, path).map_err(|source| error("rename", path, source))?;
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_data())
            .map_err(|source| error("sync", parent, source))?;
    }
    Ok(())
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}
