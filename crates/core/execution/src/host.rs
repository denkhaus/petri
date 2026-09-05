//! The standalone host: run and resume a durable run dir through the
//! coordinator.
//!
//! When petri runs a workflow with no product store behind it, the run dir is
//! the record, in the coordinator store's layout: `run.json` (identity and
//! lease), `coordinator.jsonl` (the lifecycle log), `graphs/<digest>.json` —
//! every registered graph, byte-exact — and one engine log per execution at
//! `invocations/<invocation>/executions/<execution>/events.jsonl`, streamed
//! as it happens. Everything else under the run dir — workspaces, logs, the
//! executors' own records — identifies the processes and containers of *this*
//! run and fences them on resume.
//!
//! Graphs persist byte-exact and are never masked; the contract that makes
//! that safe (§11) is that raw secret values never belong in a recorded
//! graph, and the coordinator refuses to persist durable data the masker
//! already recognizes. A lazily resolving provider can defeat the check; the
//! contract, not the check, is the rule.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use driver::ExecutionReport;
use executor::SecretProvider;
use ir::Graph;
use runtime::Runtime;

use crate::{
    Coordinator, CoordinatorError, CoordinatorHandle, CoordinatorOptions, ExecutionObserver,
    GraphDigest, InvocationId,
};

/// Each execution's engine-log file name under its execution directory.
pub const EVENTS_FILE: &str = "events.jsonl";

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
    #[error(transparent)]
    Replay(#[from] engine::ReplayMismatch),
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
    #[error("the coordinator finished the root invocation without a final execution report")]
    MissingExecutionReport,
}

/// Why `events.jsonl` bytes could not become an [`engine::EventLog`]: this
/// crate owns the framing, and the host reads and writes the same format.
pub type EventsDecodeError = crate::EngineLogDecodeError;

/// One successful `events.jsonl` decode.
pub type DecodedEvents = crate::DecodedEngineLog;

/// Decode `events.jsonl` bytes: header, records, strict torn-line rule.
pub fn decode_events(bytes: &[u8]) -> Result<DecodedEvents, EventsDecodeError> {
    crate::decode_engine_log(bytes)
}

/// Render a log in the `events.jsonl` framing, produced in one piece.
pub fn encode_events(log: &engine::EventLog) -> Vec<u8> {
    crate::encode_engine_log(log)
}

/// Read and decode an execution's `events.jsonl`.
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

/// Everything a host hands the coordinator for one fresh run: the root graph,
/// the pre-lowered child graphs a nested-workflow step may invoke (every one
/// is registered before the root starts, so an invoke by digest always
/// resolves), and the observers that see every execution's records.
pub struct HostRun {
    pub graph:     Graph,
    pub children:  Vec<Graph>,
    pub observers: Vec<Arc<dyn ExecutionObserver>>,
}

impl From<Graph> for HostRun {
    fn from(graph: Graph) -> Self {
        Self::new(graph)
    }
}

impl HostRun {
    /// A root graph alone: no children, no observers.
    pub fn new(graph: Graph) -> Self {
        Self {
            graph,
            children: Vec::new(),
            observers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_children(mut self, children: Vec<Graph>) -> Self {
        self.children = children;
        self
    }

    #[must_use]
    pub fn observe(mut self, observer: Arc<dyn ExecutionObserver>) -> Self {
        self.observers.push(observer);
        self
    }
}

/// Run a graph with the durable run dir, to completion. Every log's `finish`
/// is awaited inside the run, so the run dir is complete when this returns.
/// With the runtime's `verify_replay` on (the default), the final execution's
/// log is replayed afterwards and any divergence is the error.
pub async fn run(rt: &Runtime, graph: Graph) -> Result<ExecutionReport, HostError> {
    run_with_handle(rt, graph, |_| {}).await
}

/// [`run`], handing the coordinator's handle to `with_handle` before the run
/// starts — the hook for cancellation wiring (Ctrl-C, a deadline). The first
/// [`CoordinatorHandle::cancel_root`] cancels politely; a second reaches the
/// drivers' kill tier.
pub async fn run_with_handle(
    rt: &Runtime,
    graph: Graph,
    with_handle: impl FnOnce(CoordinatorHandle),
) -> Result<ExecutionReport, HostError> {
    run_configured(rt, HostRun::new(graph), |handle, _| with_handle(handle)).await
}

/// The general form of [`run`]: children and observers from `run`, and the
/// handle plus the run's secret provider handed to `with_handle` before the
/// run starts — the provider is how an answerer registers a dynamic secret
/// (`answer:<id>`) before delivering its reference into a live firing.
pub async fn run_configured(
    rt: &Runtime,
    run: HostRun,
    with_handle: impl FnOnce(CoordinatorHandle, Arc<dyn SecretProvider>),
) -> Result<ExecutionReport, HostError> {
    let run_dir = rt.run_options().run_dir.clone();
    let run_runtime = rt.prepare_run(&run_dir);
    let secrets = run_runtime.secret_provider();
    let mut coordinator =
        Coordinator::create(run_runtime, Vec::new(), CoordinatorOptions::default())?;
    for observer in run.observers {
        coordinator = coordinator.observe(observer);
    }
    let digest = register(&mut coordinator, &run.graph)?;
    for child in &run.children {
        register(&mut coordinator, child)?;
    }
    with_handle(coordinator.handle(), secrets);
    finish_root(rt, coordinator, digest, run.graph).await
}

fn register(coordinator: &mut Coordinator, graph: &Graph) -> Result<GraphDigest, HostError> {
    match coordinator.register_graph(graph) {
        Err(CoordinatorError::SecretInDurableData) => Err(HostError::SecretInGraph),
        result => Ok(result?),
    }
}

/// Continue the run in the runtime's run dir, to completion — the crash side
/// of [`run`]. Same guarantees, same replay verification. A torn coordinator
/// or engine-log tail truncates to the clean prefix; a record that decodes
/// wrongly refuses the resume outright.
///
/// Dynamic secrets (`answer:<id>`) are not in any log by design: re-register
/// them on the provider before delivering again, or the resumed step fails
/// with `secret_unavailable`.
pub async fn resume(rt: &Runtime) -> Result<ExecutionReport, HostError> {
    let run_dir = rt.run_options().run_dir.clone();
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
    finish_root(rt, coordinator, digest, graph).await
}

/// The shared tail of [`run`] and [`resume`]: run the root invocation to its
/// result, verify replay when the runtime asks for it, and tear the run
/// services down.
async fn finish_root(
    rt: &Runtime,
    mut coordinator: Coordinator,
    digest: GraphDigest,
    graph: Graph,
) -> Result<ExecutionReport, HostError> {
    coordinator.run_root(digest, BTreeMap::default()).await?;
    let report = coordinator
        .take_root_report()
        .ok_or(HostError::MissingExecutionReport)?;
    if rt.run_options().verify_replay {
        engine::verify_replay(graph, &report.state.log)?;
    }
    coordinator.finish().await;
    Ok(report)
}
