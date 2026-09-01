//! The standalone host: the durable run dir.
//!
//! When petri runs a workflow with no product store behind it, the run dir is
//! the record. Two files make it self-describing:
//!
//! - **`graph.json`** — the pre-splice graph, byte-exact, written once before
//!   the run. Replay needs the graph as it ran, so it is never masked; the
//!   contract that makes that safe (§11) is that raw secret values never belong
//!   in a recorded graph, and [`driver`] refuses to start when the masker
//!   already recognizes a value in the serialized bytes. A lazily resolving
//!   provider can defeat the check; the contract, not the check, is the rule.
//! - **`events.jsonl`** — every event record, streamed as it happens by the
//!   [`JsonlEventLog`] battery. Line 1 is a `{"version": N}` header, checked on
//!   read exactly as the log's own deserialization checks it (standing
//!   no-migrator policy); each further line is one record.
//!
//! This file convention is the standalone host's own, not the core's: the
//! core's whole persistence surface is `EventLog`'s serde plus
//! [`EventLog::try_from_records`]. A system with its own store persists records
//! through its own observer and never sees these files.
//!
//! These two files are the run; everything else under the run dir — workspaces,
//! logs, the executors' own records (`groups/`, `docker-run-id`) — identifies
//! the processes and containers of *this* run and fences them on resume. A fork
//! therefore copies exactly these two files into a fresh run dir, nothing else.
//!
//! The battery's durability bar is flush per record, no fsync: on process death
//! the writer's queue is a loss window — an arbitrary tail, not just one torn
//! line — and resume tolerates any lost suffix as a shorter prefix. A host that
//! needs a stronger bar owns its own sink.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use execution::{Coordinator, CoordinatorError, CoordinatorOptions, GraphDigest, InvocationId};
use runtime::Runtime;
use runtime::driver::{Driver, EventObserver, ObserveError, ResumeError, ResumeInfo, RunReport};
use runtime::engine::{self, EngineState, EventLog, EventRecord};
use runtime::executor::Masker;
use runtime::ir::{self, Graph};
use tokio::sync::oneshot;

/// The event stream's file name under the run dir.
pub const EVENTS_FILE: &str = "events.jsonl";

/// The graph's file name under the run dir.
pub const GRAPH_FILE: &str = "graph.json";

/// What kept the standalone host from running or resuming.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("could not {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`{path}`: {source}")]
    Events {
        path:   PathBuf,
        #[source]
        source: EventsDecodeError,
    },
    #[error(
        "a value the masker recognizes appears in the serialized graph; refusing to \
         persist it and start the run — raw secret values never belong in `Graph.params` \
         or a step config (§11)"
    )]
    SecretInGraph,
    #[error("could not encode the graph: {0}")]
    EncodeGraph(#[source] serde_json::Error),
    #[error("`{path}` is not a graph: {source}")]
    BadGraph {
        path:   PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// The recorded graph deserialized but fails validation — deserialization
    /// is not validation, and a resumed run must not trust `graph.json` past
    /// its syntax.
    #[error("`{path}` failed validation: {count} error(s); first: {first}")]
    InvalidGraph {
        path:  PathBuf,
        count: usize,
        first: String,
    },
    #[error(transparent)]
    Resume(#[from] ResumeError),
    #[error(transparent)]
    Replay(#[from] engine::ReplayMismatch),
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
    #[error(transparent)]
    Store(#[from] execution::StoreError),
    #[error("the coordinator finished the root invocation without a final execution report")]
    MissingExecutionReport,
}

/// Why `events.jsonl` bytes could not become an [`EventLog`]: the execution
/// crate owns the framing, and this host reads and writes the same format.
pub type EventsDecodeError = execution::EngineLogDecodeError;

/// One successful `events.jsonl` decode.
pub type DecodedEvents = execution::DecodedEngineLog;

/// Decode `events.jsonl` bytes: header, records, strict torn-line rule.
pub fn decode_events(bytes: &[u8]) -> Result<DecodedEvents, EventsDecodeError> {
    execution::decode_engine_log(bytes)
}

/// Render a log in the `events.jsonl` framing: what [`JsonlEventLog`] writes
/// incrementally, produced in one piece.
pub fn encode_events(log: &EventLog) -> Vec<u8> {
    execution::encode_engine_log(log)
}

/// Read and decode a run dir's `events.jsonl`.
pub fn read_events(path: &Path) -> Result<DecodedEvents, HostError> {
    let bytes = fs::read(path).map_err(|e| HostError::Io {
        action: "read",
        path:   path.to_path_buf(),
        source: e,
    })?;
    decode_events(&bytes).map_err(|e| HostError::Events {
        path:   path.to_path_buf(),
        source: e,
    })
}

// ── The battery ───────────────────────────────────────────────────────────

/// What reaches the writer thread.
enum Msg {
    Record(Box<EventRecord>),
    Finish(oneshot::Sender<Result<(), ObserveError>>),
}

/// The provided observer: streams every record to `events.jsonl`.
///
/// Per-run state — it holds the run's file — so the [`run`] and `resume`
/// wrappers build a fresh one per run. A writer thread fed by an unbounded
/// channel does the IO, so `on_record` never blocks the driver loop; each
/// record is flushed as it lands, without fsync. `finish` drains the queue,
/// flushes, and reports any write error.
///
/// Unbounded is deliberate: losslessness is the observer contract, and a
/// bounded channel would force `on_record` to either block the driver loop or
/// `try_send` and drop records — changing the contract, not just the buffer.
/// The memory trade is bounded by the run itself: the queue can hold at most
/// the records a run produces, which the file gets anyway, and only as far as
/// the writer trails the loop.
pub struct JsonlEventLog {
    tx:         Sender<Msg>,
    /// Records below this seq are already in the file — the resume case, where
    /// the driver redelivers at-least-once and the file is the high-water mark.
    high_water: u64,
}

impl JsonlEventLog {
    /// Start a fresh file: the header now, records as they arrive.
    pub fn create(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let mut file = File::create(&path)?;
        file.write_all(&encode_events(&EventLog::new()))?;
        file.flush()?;
        Ok(Self::over(file, path, 0))
    }

    /// Continue a file that already holds `high_water` records. The caller has
    /// already truncated any torn tail ([`DecodedEvents::clean_len`]), so every
    /// append starts on a fresh line.
    pub fn append_to(path: impl Into<PathBuf>, high_water: u64) -> io::Result<Self> {
        let path = path.into();
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self::over(file, path, high_water))
    }

    fn over(file: File, path: PathBuf, high_water: u64) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || write_records(file, &path, &rx));
        Self { tx, high_water }
    }
}

/// The writer thread: one record per line, flushed as it lands. The first write
/// failure sticks — later records are dropped and `finish` reports it.
fn write_records(mut file: File, path: &Path, rx: &Receiver<Msg>) {
    let mut failure: Option<String> = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Record(record) => {
                if failure.is_some() {
                    continue;
                }
                let result = match serde_json::to_vec(&record) {
                    Err(e) => Err(e.to_string()),
                    Ok(mut line) => {
                        line.push(b'\n');
                        file.write_all(&line)
                            .and_then(|()| file.flush())
                            .map_err(|e| e.to_string())
                    }
                };
                if let Err(message) = result {
                    // The first failure sticks, so this says it once: the log
                    // on disk is now a prefix of the run, and `finish` reports
                    // it to whoever asked for the observer.
                    tracing::warn!(
                        events_file = %path.display(),
                        error = ?message,
                        "event log write failed; later records are dropped"
                    );
                    failure = Some(format!("could not write `{}`: {message}", path.display()));
                }
            }
            Msg::Finish(reply) => {
                let result = match &failure {
                    Some(message) => Err(ObserveError::new(EVENTS_FILE, message)),
                    None => file
                        .flush()
                        .map_err(|e| ObserveError::new(EVENTS_FILE, "flush failed").with_source(e)),
                };
                let _ = reply.send(result);
            }
        }
    }
}

#[async_trait::async_trait]
impl EventObserver for JsonlEventLog {
    fn on_record(&self, record: &EventRecord, _state: &EngineState) {
        if record.seq < self.high_water {
            return;
        }
        // A dead writer thread is reported from `finish`, not here.
        let _ = self.tx.send(Msg::Record(Box::new(record.clone())));
    }

    async fn finish(&self) -> Result<(), ObserveError> {
        let (reply, done) = oneshot::channel();
        let dead = || ObserveError::new(EVENTS_FILE, "the writer thread died");
        if self.tx.send(Msg::Finish(reply)).is_err() {
            return Err(dead());
        }
        done.await.unwrap_or_else(|_| Err(dead()))
    }
}

// ── Running with the run dir ──────────────────────────────────────────────

/// Serialize the graph, refusing if the masker would change the bytes: a value
/// it recognizes is a resolved secret, and a secret must never be persisted —
/// masked or not, since a masked graph would be a different graph under replay.
fn encode_graph_checked(graph: &Graph, masker: &Masker) -> Result<Vec<u8>, HostError> {
    let encoded = serde_json::to_string(graph).map_err(HostError::EncodeGraph)?;
    if masker.mask(&encoded) != encoded {
        return Err(HostError::SecretInGraph);
    }
    Ok(encoded.into_bytes())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), HostError> {
    fs::write(path, bytes).map_err(|e| HostError::Io {
        action: "write",
        path:   path.to_path_buf(),
        source: e,
    })
}

/// A driver over the runtime's configuration with the run dir persisted:
/// `graph.json` written now (after the known-secret check, against the
/// runtime's own mask set), a fresh [`JsonlEventLog`] battery attached. [`run`]
/// is the plain path; use this when you need the [`Driver::handle`] before
/// running.
pub fn driver(rt: &Runtime, graph: Graph) -> Result<Driver, HostError> {
    let run_dir = rt.run_options().run_dir.clone();
    fs::create_dir_all(&run_dir).map_err(|e| HostError::Io {
        action: "create",
        path:   run_dir.clone(),
        source: e,
    })?;
    let encoded = encode_graph_checked(&graph, &rt.masker())?;
    write_file(&run_dir.join(GRAPH_FILE), &encoded)?;
    let events = run_dir.join(EVENTS_FILE);
    let battery = JsonlEventLog::create(&events).map_err(|e| HostError::Io {
        action: "create",
        path:   events,
        source: e,
    })?;
    Ok(rt.driver(graph).observe(Arc::new(battery)))
}

/// Run a graph with the durable run dir, to completion. The battery's `finish`
/// is awaited inside the run, so both files are complete when this returns;
/// write failures are in `RunReport::observer_errors`. With the runtime's
/// `verify_replay` on (the default), the log is replayed afterwards and any
/// divergence is the error.
pub async fn run(rt: &Runtime, graph: Graph) -> Result<RunReport, HostError> {
    let run_dir = rt.run_options().run_dir.clone();
    let run_runtime = rt.prepare_run(&run_dir);
    let mut coordinator =
        Coordinator::create(run_runtime, Vec::new(), CoordinatorOptions::default())?;
    let digest = match coordinator.register_graph(&graph) {
        Err(CoordinatorError::SecretInDurableData) => return Err(HostError::SecretInGraph),
        result => result?,
    };
    finish_root(rt, &run_dir, coordinator, digest, graph).await
}

fn read_graph(rt: &Runtime) -> Result<Graph, HostError> {
    let path = rt.run_options().run_dir.join(GRAPH_FILE);
    let bytes = fs::read(&path).map_err(|e| HostError::Io {
        action: "read",
        path:   path.clone(),
        source: e,
    })?;
    serde_json::from_slice(&bytes).map_err(|e| HostError::BadGraph { path, source: e })
}

/// A driver continuing the run in the runtime's run dir: `graph.json` and
/// `events.jsonl` loaded back, any EOF-torn tail truncated away (a shorter
/// prefix, by the strict rule), and a battery that appends to the same file —
/// the regenerated suffix converges it without rewriting history. [`resume`] is
/// the plain path.
///
/// Dynamic secrets (`answer:<id>`) are not in the log by design: re-register
/// them on the provider before delivering again, or the resumed step fails with
/// `secret_unavailable`.
pub fn resume_driver(rt: &Runtime) -> Result<(Driver, ResumeInfo), HostError> {
    resume_over(rt, read_graph(rt)?)
}

/// [`resume_driver`] past the graph read, so [`resume`] can keep the graph it
/// read for verification.
fn resume_over(rt: &Runtime, graph: Graph) -> Result<(Driver, ResumeInfo), HostError> {
    let run_dir = rt.run_options().run_dir.clone();
    // A fresh run's graph was validated when it lowered; the recorded one gets
    // the same full check here — structure and step registry both, since
    // nothing else has looked at these bytes.
    if let Err(errors) = ir::validate_with(&graph, Some(rt.registry())) {
        return Err(HostError::InvalidGraph {
            path:  run_dir.join(GRAPH_FILE),
            count: errors.len(),
            first: errors[0].to_string(),
        });
    }
    // The §11 contract holds on resume too; a refusal here is a refusal to
    // continue, not to write.
    encode_graph_checked(&graph, &rt.masker())?;

    let events = run_dir.join(EVENTS_FILE);
    let decoded = read_events(&events)?;
    if decoded.torn {
        // The run died mid-write. The truncated tail is the run's own history,
        // so say so before the file changes.
        tracing::warn!(
            events_file = %events.display(),
            clean_bytes = decoded.clean_len,
            record_count = decoded.log.len(),
            "truncating an EOF-torn tail before resume"
        );
        let file = OpenOptions::new()
            .write(true)
            .open(&events)
            .map_err(|e| HostError::Io {
                action: "open",
                path:   events.clone(),
                source: e,
            })?;
        file.set_len(decoded.clean_len as u64)
            .map_err(|e| HostError::Io {
                action: "truncate",
                path:   events.clone(),
                source: e,
            })?;
    }
    let loaded = decoded.log.len() as u64;
    let (driver, info) = rt.resume_driver(graph, decoded.log)?;
    let battery = JsonlEventLog::append_to(&events, loaded).map_err(|e| HostError::Io {
        action: "open",
        path:   events,
        source: e,
    })?;
    Ok((driver.observe(Arc::new(battery)), info))
}

/// Continue the run in the runtime's run dir, to completion — the crash side of
/// [`run`]. Same file guarantees, same replay verification.
pub async fn resume(rt: &Runtime) -> Result<RunReport, HostError> {
    let run_dir = rt.run_options().run_dir.clone();
    if !run_dir.join(execution::RUN_FILE).exists() {
        let graph = read_graph(rt)?;
        return rt
            .run_verified(graph, |graph| {
                resume_over(rt, graph).map(|(driver, _info)| driver)
            })
            .await;
    }

    let run_runtime = rt.prepare_run(&run_dir);
    let (mut coordinator, torn) =
        Coordinator::resume(run_runtime, Vec::new(), CoordinatorOptions::default())?;
    if torn {
        tracing::warn!("truncated an EOF-torn coordinator record before resume");
    }
    let digest = coordinator.store().state().invocations[&InvocationId::ROOT]
        .declaration
        .graph;
    let graph = (*coordinator.load_graph(digest)?).clone();
    finish_root(rt, &run_dir, coordinator, digest, graph).await
}

/// The shared tail of [`run`] and [`resume`]: run the root invocation to its
/// result, verify replay when the runtime asks for it, refresh the legacy
/// mirror, and tear the run services down.
async fn finish_root(
    rt: &Runtime,
    run_dir: &Path,
    mut coordinator: Coordinator,
    digest: GraphDigest,
    graph: Graph,
) -> Result<RunReport, HostError> {
    coordinator.run_root(digest, BTreeMap::default()).await?;
    let report = coordinator
        .take_root_report()
        .ok_or(HostError::MissingExecutionReport)?;
    if rt.run_options().verify_replay {
        engine::verify_replay(graph, &report.state.log)?;
    }
    mirror_legacy_files(run_dir, digest, &coordinator)?;
    coordinator.finish().await;
    Ok(report)
}

/// Refresh the read-only legacy mirror — `graph.json` plus a copy of the root
/// execution's engine log — beside the coordinator layout. Purely an export:
/// resume never reads it back.
fn mirror_legacy_files(
    run_dir: &Path,
    graph: GraphDigest,
    coordinator: &Coordinator,
) -> Result<(), HostError> {
    write_file(
        &run_dir.join(GRAPH_FILE),
        &coordinator.store().graph_bytes(graph)?,
    )?;
    let root = &coordinator.store().state().invocations[&InvocationId::ROOT];
    let execution = *root
        .executions
        .last()
        .ok_or(HostError::MissingExecutionReport)?;
    let source = coordinator
        .store()
        .execution_dir(InvocationId::ROOT, execution)
        .join(EVENTS_FILE);
    fs::copy(&source, run_dir.join(EVENTS_FILE)).map_err(|source_error| HostError::Io {
        action: "copy",
        path:   source,
        source: source_error,
    })?;
    Ok(())
}
