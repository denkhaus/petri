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

use crate::breaker::CircuitBreaker;
use crate::{
    Coordinator, CoordinatorError, CoordinatorHandle, CoordinatorOptions, CoordinatorState,
    ExecutionObserver, GraphDigest, InvocationId, Middleware, decode_coordinator_log,
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
    pub graph:      Graph,
    pub children:   Vec<Graph>,
    pub observers:  Vec<Arc<dyn ExecutionObserver>>,
    /// Decision middleware the host adds after the graph's own policy chain
    /// ([`policy_middleware`]): a pause gate, a product's routing hooks. A
    /// resume must install the same list.
    pub middleware: Vec<Arc<dyn Middleware>>,
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
            middleware: Vec::new(),
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

    #[must_use]
    pub fn with_middleware(mut self, middleware: Arc<dyn Middleware>) -> Self {
        self.middleware.push(middleware);
        self
    }
}

/// The decision middleware a root graph's [`ir::RunPolicy`] asks for: the
/// failure circuit breaker when `loop_restart_signature_limit` is set. The
/// standalone host installs it ahead of the host's own middleware on run and
/// on resume, so the recorded chain matches.
pub fn policy_middleware(graph: &Graph) -> Vec<Arc<dyn Middleware>> {
    let mut chain: Vec<Arc<dyn Middleware>> = Vec::new();
    if let Some(limit) = graph.policy.loop_restart_signature_limit {
        chain.push(Arc::new(CircuitBreaker::reference(limit)));
    }
    chain
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
    let mut chain = policy_middleware(&run.graph);
    chain.extend(run.middleware);
    let mut coordinator = Coordinator::create(run_runtime, chain, CoordinatorOptions::default())?;
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
    resume_configured(rt, Vec::new(), Vec::new(), |_, _| {}).await
}

/// [`resume`] with the host's observers, its own middleware (the same list
/// the run was started with, after the graph's policy chain), and the handle
/// hook. The policy chain is rebuilt from the stored root graph before the
/// coordinator checks the recorded chain.
pub async fn resume_configured(
    rt: &Runtime,
    middleware: Vec<Arc<dyn Middleware>>,
    observers: Vec<Arc<dyn ExecutionObserver>>,
    with_handle: impl FnOnce(CoordinatorHandle, Arc<dyn SecretProvider>),
) -> Result<ExecutionReport, HostError> {
    let run_dir = rt.run_options().run_dir.clone();
    let root_graph = stored_root_graph(&run_dir)?;
    let mut chain = root_graph
        .as_ref()
        .map(policy_middleware)
        .unwrap_or_default();
    chain.extend(middleware);
    let run_runtime = rt.prepare_run(&run_dir);
    let secrets = run_runtime.secret_provider();
    let (mut coordinator, torn) =
        Coordinator::resume(run_runtime, chain, CoordinatorOptions::default())?;
    if torn {
        tracing::warn!("truncated an EOF-torn coordinator record before resume");
    }
    for observer in observers {
        coordinator = coordinator.observe(observer);
    }
    let digest = coordinator.store().state().invocations[&InvocationId::ROOT]
        .declaration
        .graph;
    let graph = (*coordinator.load_graph(digest)?).clone();
    with_handle(coordinator.handle(), secrets);
    finish_root(rt, coordinator, digest, graph).await
}

/// The root invocation's registered graph, read without taking the run
/// lease: what a resume needs before the coordinator exists. `None` when the
/// log has no root invocation yet.
fn stored_root_graph(run_dir: &Path) -> Result<Option<Graph>, HostError> {
    let log_path = run_dir.join(crate::COORDINATOR_FILE);
    let bytes = fs::read(&log_path).map_err(|e| HostError::Io {
        action: "read",
        path:   log_path.clone(),
        source: e,
    })?;
    let decoded = decode_coordinator_log(&log_path, &bytes).map_err(CoordinatorError::from)?;
    let state = CoordinatorState::replay(&decoded.records)
        .map_err(|error| CoordinatorError::from(crate::StoreError::State(error)))?;
    let Some(root) = state.invocations.get(&InvocationId::ROOT) else {
        return Ok(None);
    };
    let path = run_dir
        .join(crate::GRAPHS_DIR)
        .join(format!("{}.json", root.declaration.graph));
    let bytes = fs::read(&path).map_err(|e| HostError::Io {
        action: "read",
        path:   path.clone(),
        source: e,
    })?;
    let graph: Graph = serde_json::from_slice(&bytes).map_err(|error| HostError::Io {
        action: "decode",
        path,
        source: io::Error::new(io::ErrorKind::InvalidData, error),
    })?;
    Ok(Some(graph))
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
