use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::{fmt, io};

use driver::{EventObserver as _, SandboxAssignment, ScopeLease, ScopeLeaseAllocator, ScopeLeases};
use engine::{EngineExit, EngineStart, EntryPoint, Event, MiddlewareKey};
use executor_sandbox::{CONTAINER_KIND, RoutingExecutor};
use ir::{
    Control, FailureClass, FailureInfo, FiringId, Graph, ResultProjection, RunStatus,
    RuntimeTarget, ScopeId, Value,
};
use runtime::RunRuntime;
use smol_str::SmolStr;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinError, JoinSet, spawn_blocking};

use crate::client::StartRequest;
use crate::host::EVENTS_FILE;
use crate::middleware::derive_fold_event;
use crate::{
    CoordinatorEvent, CoordinatorInvocationClient, CoordinatorRecord, CoordinatorStore,
    EngineLogError, ExecutionId, ExecutionObserver, GraphDigest, HOST_PROVIDER, InvocationHandle,
    InvocationId, InvocationResult, InvocationSecrets, InvocationStatus, InvokeError,
    JsonlEngineLog, Middleware, MiddlewarePipeline, MiddlewareState, ParentCallKey, ResourceError,
    ResourceLedger, ResourceStore, SandboxAllocationKey, SandboxBinding, SandboxMode,
    SecretBindings, StoreError, initial_middleware_state, read_engine_log,
};

pub const DEFAULT_MAX_INVOCATIONS: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoordinatorOptions {
    pub max_invocations: u32,
    pub max_executions:  u32,
}

impl Default for CoordinatorOptions {
    fn default() -> Self {
        Self {
            max_invocations: DEFAULT_MAX_INVOCATIONS,
            max_executions:  engine::DEFAULT_MAX_EXECUTIONS,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    EngineLog(#[from] EngineLogError),
    #[error(transparent)]
    Resume(#[from] driver::ResumeError),
    #[error(transparent)]
    Resource(#[from] ResourceError),
    #[error("unknown graph {0}")]
    UnknownGraph(GraphDigest),
    #[error("the root invocation is already declared with a different request")]
    RootRequestMismatch,
    #[error("maximum total invocations per run reached")]
    InvocationLimit,
    #[error("the calling firing has no inheritable sandbox")]
    NoInheritableSandbox,
    #[error("sandbox lease {lease} does not match its invocation graph")]
    InvalidResource { lease: crate::SandboxLeaseId },
    #[error("invocation {0} has no execution")]
    MissingExecution(InvocationId),
    #[error("execution {0} finished without an engine exit")]
    MissingExit(ExecutionId),
    #[error("invocation {invocation} reached its execution limit")]
    ExecutionLimit { invocation: InvocationId },
    #[error("execution {execution} log disagrees with the coordinator log")]
    ConflictingExit { execution: ExecutionId },
    #[error("execution {execution} event writer failed: {message}")]
    EventWriter {
        execution: ExecutionId,
        message:   String,
    },
    #[error("execution task failed: {0}")]
    ExecutionTask(#[from] JoinError),
    #[error("a resolved secret appears in durable invocation data")]
    SecretInDurableData,
    #[error("could not inspect durable invocation data: {0}")]
    EncodeDurableData(#[source] serde_json::Error),
    #[error("could not {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What a start request resolved to.
enum StartOutcome {
    /// Wait for a superseded prior attempt to settle, then requeue the
    /// request.
    Requeue { previous: InvocationId },
    /// Attach to this invocation — newly declared, or found by its call key.
    Attach {
        invocation: InvocationId,
        is_new:     bool,
    },
}

struct PreparedExecution {
    driver:            driver::Driver,
    pipeline:          Arc<MiddlewarePipeline>,
    cancel_before_run: bool,
}

/// A driver has stopped; its invocation can finish after its descendants
/// settle.
struct CompletedExecution {
    invocation:       InvocationId,
    execution:        ExecutionId,
    graph:            Arc<Graph>,
    report:           driver::ExecutionReport,
    middleware_state: MiddlewareState,
}

type ExecutionLeases = Arc<Mutex<BTreeMap<ScopeId, crate::SandboxLeaseId>>>;

/// The coordinator delegates reservation to each execution's acquire tasks.
/// The shared resource store still serializes lease ID allocation and writes.
struct InvocationLeaseAllocator {
    invocation: InvocationId,
    execution:  ExecutionId,
    resources:  Arc<Mutex<ResourceStore>>,
    acquired:   ExecutionLeases,
    router:     Option<Arc<RoutingExecutor>>,
    writer:     Arc<JsonlEngineLog>,
}

impl fmt::Debug for InvocationLeaseAllocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InvocationLeaseAllocator")
            .field("invocation", &self.invocation)
            .field("execution", &self.execution)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl ScopeLeaseAllocator for InvocationLeaseAllocator {
    async fn reserve(
        &self,
        identity: engine::ScopeIdentity,
        spec: &executor::ScopeSpec,
    ) -> Result<ScopeLease, executor::EnvError> {
        let error = |error: &dyn fmt::Display| {
            executor::EnvError::backend("coordinator", "reserve lease", error.to_string())
        };
        let introduced_by =
            matches!(identity, engine::ScopeIdentity::Spliced(_)).then_some(self.execution);
        // The scope's introducing outcome must be durable before its lease.
        // This queues a sync behind all records already observed by the driver.
        if introduced_by.is_some() {
            self.writer
                .finish()
                .await
                .map_err(|source| error(&source))?;
        }
        let provider = self
            .router
            .as_ref()
            .map_or_else(
                || match spec.runtime.target {
                    RuntimeTarget::HostProcess => HOST_PROVIDER,
                    RuntimeTarget::Container { .. } => CONTAINER_KIND,
                },
                |router| router.provider_kind_for(&spec.runtime),
            )
            .to_owned();
        let allocation = SandboxAllocationKey {
            invocation: self.invocation,
            scope:      identity,
        };
        let resources = self.resources.clone();
        let runtime = spec.runtime.clone();
        let assignment = spawn_blocking(move || {
            let mut resources = resources.lock().unwrap_or_else(PoisonError::into_inner);
            let record = resources.reserve_scope(allocation, &provider, runtime, introduced_by)?;
            if record.state == crate::LeaseState::Deleted {
                return Err(ResourceError::DeletedLease(record.lease));
            }
            Ok(ScopeLease {
                lease:     record.lease,
                workspace: record.workspace.clone(),
            })
        })
        .await
        .map_err(|source| error(&source))?
        .map_err(|source| error(&source))?;
        self.acquired
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(spec.id, assignment.lease);
        Ok(assignment)
    }
}

/// A coordinator failure rendered for the invoking step.
fn invoke_error(error: impl fmt::Display) -> InvokeError {
    InvokeError::Coordinator(SmolStr::new(error.to_string()))
}

struct ControlRequest {
    execution: ExecutionId,
    firing:    FiringId,
    control:   Control,
    reply:     oneshot::Sender<driver::DeliverDisposition>,
}

/// A cloneable control path for a coordinator that is currently running.
#[derive(Clone)]
pub struct CoordinatorHandle {
    cancel:  mpsc::UnboundedSender<InvocationId>,
    control: mpsc::UnboundedSender<ControlRequest>,
}

impl CoordinatorHandle {
    /// Politely cancel an invocation and every active descendant.
    pub fn cancel(&self, invocation: InvocationId) {
        let _ = self.cancel.send(invocation);
    }

    /// Politely cancel the complete root run.
    pub fn cancel_root(&self) {
        self.cancel(InvocationId::ROOT);
    }

    /// Deliver a control to one execution-local firing.
    pub async fn deliver(
        &self,
        execution: ExecutionId,
        firing: FiringId,
        control: Control,
    ) -> driver::DeliverDisposition {
        let (reply, result) = oneshot::channel();
        if self
            .control
            .send(ControlRequest {
                execution,
                firing,
                control,
                reply,
            })
            .is_err()
        {
            return driver::DeliverDisposition::NotLive;
        }
        result.await.unwrap_or(driver::DeliverDisposition::NotLive)
    }
}

/// Owns one run's lifecycle log, engines, resource boundary, and observers.
pub struct Coordinator {
    store:            CoordinatorStore,
    runtime:          RunRuntime,
    options:          CoordinatorOptions,
    observers:        Vec<Arc<dyn ExecutionObserver>>,
    start_tx:         mpsc::Sender<StartRequest>,
    start_rx:         mpsc::Receiver<StartRequest>,
    cancel_tx:        mpsc::UnboundedSender<InvocationId>,
    cancel_rx:        mpsc::UnboundedReceiver<InvocationId>,
    control_tx:       mpsc::UnboundedSender<ControlRequest>,
    control_rx:       mpsc::UnboundedReceiver<ControlRequest>,
    statuses:         BTreeMap<InvocationId, watch::Sender<InvocationStatus>>,
    active:           BTreeSet<InvocationId>,
    active_handles:   BTreeMap<ExecutionId, (InvocationId, driver::RunHandle)>,
    middleware:       Vec<Arc<dyn Middleware>>,
    last_root_report: Option<driver::ExecutionReport>,
    /// The durable lease records, shared with the executor's lease manager
    /// as its ledger.
    resources:        Arc<Mutex<ResourceStore>>,
    execution_leases: BTreeMap<ExecutionId, ExecutionLeases>,
    #[cfg(test)]
    release_gate:     Option<Arc<tests::ReleaseGate>>,
}

impl Coordinator {
    /// Start a fresh run. `middleware` may be empty; the configured chain's
    /// keys are recorded durably either way.
    pub fn create(
        runtime: RunRuntime,
        middleware: Vec<Arc<dyn Middleware>>,
        options: CoordinatorOptions,
    ) -> Result<Self, CoordinatorError> {
        let keys = middleware.iter().map(|item| item.key()).collect();
        let store = CoordinatorStore::create(runtime.run_dir(), keys)?;
        let resources = ResourceStore::load(runtime.run_dir().join(crate::RESOURCES_DIR))?;
        Ok(Self::assemble(
            store, resources, runtime, middleware, options,
        ))
    }

    /// Resume a crashed run. `middleware` must match the recorded chain.
    pub fn resume(
        runtime: RunRuntime,
        middleware: Vec<Arc<dyn Middleware>>,
        options: CoordinatorOptions,
    ) -> Result<(Self, bool), CoordinatorError> {
        let (mut store, torn) = CoordinatorStore::resume(runtime.run_dir())?;
        let resources = ResourceStore::load(runtime.run_dir().join(crate::RESOURCES_DIR))?;
        let keys: Vec<MiddlewareKey> = middleware.iter().map(|item| item.key()).collect();
        if store.state().middleware_chain != keys {
            return Err(StoreError::State(crate::StateError::MiddlewareChain).into());
        }
        validate_resources(&mut store, &resources)?;
        for lease in store.state().invocations.values().filter_map(|invocation| {
            match invocation.declaration.sandbox {
                SandboxBinding::Inherited { lease } => Some(lease),
                SandboxBinding::Isolated => None,
            }
        }) {
            resources.resolve(lease)?;
        }
        Ok((
            Self::assemble(store, resources, runtime, middleware, options),
            torn,
        ))
    }

    fn assemble(
        store: CoordinatorStore,
        resources: ResourceStore,
        runtime: RunRuntime,
        middleware: Vec<Arc<dyn Middleware>>,
        options: CoordinatorOptions,
    ) -> Self {
        let (start_tx, start_rx) = mpsc::channel(128);
        let (cancel_tx, cancel_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        // The records are the executor's ledger from here on: every
        // container scope it allocates is written here before it exists.
        let resources = Arc::new(Mutex::new(resources));
        runtime.attach_lease_ledger(Arc::new(ResourceLedger::new(resources.clone())));
        Self {
            store,
            runtime,
            options,
            observers: Vec::new(),
            start_tx,
            start_rx,
            cancel_tx,
            cancel_rx,
            control_tx,
            control_rx,
            statuses: BTreeMap::new(),
            active: BTreeSet::new(),
            active_handles: BTreeMap::new(),
            middleware,
            last_root_report: None,
            resources,
            execution_leases: BTreeMap::new(),
            #[cfg(test)]
            release_gate: None,
        }
    }

    /// Register an observer of every execution's records and every
    /// coordinator record. A fresh run's opening record (`RunStarted`) was
    /// appended before any observer could attach; it is delivered here, so
    /// the observer sees the coordinator log from its first record.
    #[must_use]
    pub fn observe(mut self, observer: Arc<dyn ExecutionObserver>) -> Self {
        for record in self.store.opening_records() {
            observer.on_lifecycle(record);
        }
        self.observers.push(observer);
        self
    }

    pub fn store(&self) -> &CoordinatorStore {
        &self.store
    }

    /// A registered graph, decoded and validated once and cached by digest.
    pub fn load_graph(&mut self, digest: GraphDigest) -> Result<Arc<Graph>, CoordinatorError> {
        Ok(self.store.load_graph(digest)?)
    }

    pub fn take_root_report(&mut self) -> Option<driver::ExecutionReport> {
        self.last_root_report.take()
    }

    pub fn handle(&self) -> CoordinatorHandle {
        CoordinatorHandle {
            cancel:  self.cancel_tx.clone(),
            control: self.control_tx.clone(),
        }
    }

    pub fn register_graph(&mut self, graph: &Graph) -> Result<GraphDigest, CoordinatorError> {
        let encoded = serde_json::to_string(graph).map_err(CoordinatorError::EncodeDurableData)?;
        self.refuse_secret_bytes(&encoded)?;
        let (digest, record) = self.store.register_graph_bytes(encoded.as_bytes())?;
        if let Some(record) = record {
            for observer in &self.observers {
                observer.on_lifecycle(&record);
            }
        }
        Ok(digest)
    }

    pub async fn run_root(
        &mut self,
        graph: GraphDigest,
        context: BTreeMap<SmolStr, Value>,
    ) -> Result<InvocationResult, CoordinatorError> {
        if !self.store.state().graphs.contains(&graph) {
            return Err(CoordinatorError::UnknownGraph(graph));
        }
        self.refuse_secret(&context)?;
        if self
            .store
            .state()
            .invocations
            .contains_key(&InvocationId::ROOT)
        {
            let declaration = &self.store.state().invocations[&InvocationId::ROOT].declaration;
            if declaration.graph != graph || declaration.context != context {
                return Err(CoordinatorError::RootRequestMismatch);
            }
        } else {
            self.append(CoordinatorEvent::InvocationDeclared {
                invocation: InvocationId::ROOT,
                call: None,
                graph,
                context,
                secret_bindings: SecretBindings::None,
                sandbox: SandboxBinding::Isolated,
            })?;
        }

        if let Some(result) = self.store.state().invocations[&InvocationId::ROOT]
            .result
            .clone()
        {
            let execution = result.final_execution;
            let declaration = self.store.state().executions[&execution]
                .declaration
                .clone();
            let registered = self.store.load_graph(graph)?;
            let recorded = self.store.state().executions[&execution]
                .exit
                .clone()
                .ok_or(CoordinatorError::MissingExit(execution))?;
            let PreparedExecution { driver, .. } = self.prepare_execution(
                InvocationId::ROOT,
                execution,
                &declaration.start,
                declaration.middleware_state,
                &registered,
            )?;
            let report = driver.run().await;
            Self::check_report(execution, &report)?;
            if report.exit != recorded {
                return Err(CoordinatorError::ConflictingExit { execution });
            }
            self.last_root_report = Some(report);
            let settled = self.run_invocations().await;
            self.active.clear();
            self.active_handles.clear();
            settled?;
            return Ok(result);
        }
        let result = self.run_invocations().await;
        self.active.clear();
        self.active_handles.clear();
        let result = result?;
        if self.store.state().run_status.is_none() {
            self.append(CoordinatorEvent::RunFinished {
                status: result.status,
            })?;
        }
        Ok(result)
    }

    /// End the run: release every lease still holding a sandbox — an
    /// invocation that finished before a crash, or one that never finished
    /// — with the run's own status, then tear the run services down.
    pub async fn finish(self) {
        let status = self
            .store
            .state()
            .run_status
            .unwrap_or(RunStatus::Cancelled);
        let remaining: Vec<_> = self
            .resources()
            .records()
            .filter(|record| record.needs_release())
            .map(|record| {
                let owner_status = self
                    .store
                    .state()
                    .invocations
                    .get(&record.allocation.invocation)
                    .and_then(|invocation| invocation.result.as_ref())
                    .map_or(status, |result| result.status);
                (record.lease, owner_status)
            })
            .collect();
        for (lease, owner_status) in remaining {
            self.release_lease(lease, owner_status).await;
        }
        self.runtime.finish().await;
    }

    fn resources(&self) -> MutexGuard<'_, ResourceStore> {
        self.resources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Stop a lease's sandbox, then keep or delete it by retention for the
    /// outcome `status` maps to. Release is best effort; a problem is
    /// logged, and the record keeps its pending intent for the next attempt
    /// (`finish`, or `petri sandbox prune`).
    async fn release_lease(&self, lease: crate::SandboxLeaseId, status: RunStatus) {
        let outcome = if status == RunStatus::Success {
            executor::ScopeOutcome::Succeeded
        } else {
            executor::ScopeOutcome::Failed
        };
        let report = self.runtime.release_lease(lease, outcome).await;
        for problem in &report.problems {
            tracing::warn!(
                lease = lease.raw(),
                problem,
                "sandbox lease release problem"
            );
        }
    }

    /// Release the leases `invocation` allocated, now that it has finished.
    /// An inherited invocation allocated none: its caller's lease outlives
    /// it.
    fn release_invocation_leases(
        &mut self,
        invocation: InvocationId,
        status: RunStatus,
        releasing: &mut JoinSet<InvocationId>,
    ) {
        let owned: Vec<crate::SandboxLeaseId> = self
            .resources()
            .records()
            .filter(|record| record.allocation.invocation == invocation && record.needs_release())
            .map(|record| record.lease)
            .collect();
        let router = self.runtime.sandbox_router().cloned();
        #[cfg(test)]
        let gate = self.release_gate.take();
        releasing.spawn(async move {
            #[cfg(test)]
            if let Some(gate) = gate {
                gate.started.notify_one();
                gate.complete.notified().await;
            }
            if let Some(router) = router {
                let outcome = if status == RunStatus::Success {
                    executor::ScopeOutcome::Succeeded
                } else {
                    executor::ScopeOutcome::Failed
                };
                for lease in owned {
                    let report = router.release_lease(lease, outcome).await;
                    for problem in report.problems {
                        tracing::warn!(
                            lease = lease.raw(),
                            problem,
                            "sandbox lease release problem"
                        );
                    }
                }
            }
            invocation
        });
    }

    /// Service every execution from one loop. A request can come from any live
    /// driver, so scheduling it must not make it a child of another sibling.
    async fn run_invocations(&mut self) -> Result<InvocationResult, CoordinatorError> {
        let mut running = JoinSet::new();
        let mut releasing = JoinSet::new();
        let result = self.drive_invocations(&mut running, &mut releasing).await;
        // On an error, wait for the aborted driver futures to drop before
        // the caller continues with run cleanup.
        running.shutdown().await;
        releasing.shutdown().await;
        result
    }

    async fn drive_invocations(
        &mut self,
        running: &mut JoinSet<CompletedExecution>,
        releasing: &mut JoinSet<InvocationId>,
    ) -> Result<InvocationResult, CoordinatorError> {
        let mut completed = BTreeMap::new();
        if self.store.state().invocations[&InvocationId::ROOT]
            .result
            .is_some()
        {
            self.settle_descendants(InvocationId::ROOT, running).await?;
        } else {
            self.start_invocation(InvocationId::ROOT, running)?;
        }

        loop {
            if self.active.is_empty()
                && let Some(result) = self.store.state().invocations[&InvocationId::ROOT]
                    .result
                    .clone()
            {
                return Ok(result);
            }
            tokio::select! {
                result = running.join_next(), if !running.is_empty() => {
                    let done = result.expect("the execution set is not empty")?;
                    self.active_handles.remove(&done.execution);
                    self.execution_leases.remove(&done.execution);
                    Self::check_report(done.execution, &done.report)?;
                    self.settle_descendants(done.invocation, running).await?;
                    completed.insert(done.invocation, done);
                }
                result = releasing.join_next(), if !releasing.is_empty() => {
                    let invocation = result.expect("the release set is not empty")?;
                    self.active.remove(&invocation);
                    if let Some(sender) = self.statuses.get(&invocation) {
                        let result = self.store.state().invocations[&invocation].result.clone()
                            .expect("release follows the durable invocation result");
                        sender.send_replace(InvocationStatus::Finished(result));
                    }
                }
                request = self.start_rx.recv() => {
                    if let Some(request) = request
                        && let Some(invocation) = self.handle_start(request).await?
                    {
                        self.start_invocation(invocation, running)?;
                    }
                }
                cancelled = self.cancel_rx.recv() => {
                    if let Some(cancelled) = cancelled {
                        self.handle_cancel(cancelled).await?;
                    }
                }
                request = self.control_rx.recv() => {
                    if let Some(request) = request {
                        self.handle_control(request);
                    }
                }
            }

            // Finishing a descendant can make a waiting parent ready too.
            while let Some(invocation) = completed.keys().copied().find(|invocation| {
                !self.active.iter().any(|candidate| {
                    candidate != invocation && self.is_descendant_or_same(*candidate, *invocation)
                })
            }) {
                let done = completed.remove(&invocation).expect("completed invocation");
                self.complete_execution(done, running, releasing)?;
            }
        }
    }

    /// A terminal replay need not reissue its old calls. Settle all durable
    /// descendants before releasing the parent's resources or restarting it.
    async fn settle_descendants(
        &mut self,
        invocation: InvocationId,
        running: &mut JoinSet<CompletedExecution>,
    ) -> Result<(), CoordinatorError> {
        let descendants: Vec<_> = self
            .store
            .state()
            .invocations
            .iter()
            .filter_map(|(candidate, state)| {
                (*candidate != invocation
                    && state.result.is_none()
                    && self.is_descendant_or_same(*candidate, invocation))
                .then_some((*candidate, state.cancelled))
            })
            .collect();
        let uncancelled = descendants
            .iter()
            .copied()
            .filter(|(_, cancelled)| !cancelled)
            .collect();
        self.cancel_invocations(uncancelled).await?;
        for (descendant, _) in descendants {
            self.start_invocation(descendant, running)?;
        }
        Ok(())
    }

    fn start_invocation(
        &mut self,
        invocation: InvocationId,
        running: &mut JoinSet<CompletedExecution>,
    ) -> Result<(), CoordinatorError> {
        let state = &self.store.state().invocations[&invocation];
        if state.result.is_some() || self.active.contains(&invocation) {
            return Ok(());
        }
        if state.executions.is_empty() {
            self.declare_first_execution(invocation)?;
        }
        let execution = *self.store.state().invocations[&invocation]
            .executions
            .last()
            .ok_or(CoordinatorError::MissingExecution(invocation))?;
        if let Some(status) = self.statuses.get(&invocation) {
            status.send_replace(InvocationStatus::Running { execution });
        }
        let declaration = self.store.state().executions[&execution]
            .declaration
            .clone();
        let graph = self.store.load_graph(
            self.store.state().invocations[&invocation]
                .declaration
                .graph,
        )?;
        let PreparedExecution {
            driver,
            pipeline,
            cancel_before_run,
        } = self.prepare_execution(
            invocation,
            execution,
            &declaration.start,
            declaration.middleware_state,
            &graph,
        )?;
        let handle = driver.handle();
        self.active.insert(invocation);
        self.active_handles
            .insert(execution, (invocation, handle.clone()));
        running.spawn(async move {
            if cancel_before_run {
                handle.cancel(ir::CancelScopeId::ROOT).await;
            }
            let report = driver.run().await;
            CompletedExecution {
                invocation,
                execution,
                graph,
                report,
                middleware_state: pipeline.checkpoint(),
            }
        });
        Ok(())
    }

    fn complete_execution(
        &mut self,
        done: CompletedExecution,
        running: &mut JoinSet<CompletedExecution>,
        releasing: &mut JoinSet<InvocationId>,
    ) -> Result<(), CoordinatorError> {
        let CompletedExecution {
            invocation,
            execution,
            graph,
            report,
            middleware_state,
        } = done;
        let exit = report.exit.clone();
        if let Some(recorded) = &self.store.state().executions[&execution].exit {
            if recorded != &exit {
                return Err(CoordinatorError::ConflictingExit { execution });
            }
        } else {
            self.append(CoordinatorEvent::ExecutionFinished {
                execution,
                exit: exit.clone(),
            })?;
        }
        match exit {
            EngineExit::Restart { target, .. } => {
                self.active.remove(&invocation);
                if self.store.state().successor_of(execution).is_none() {
                    self.declare_successor(
                        invocation,
                        execution,
                        target,
                        // Only the registered graph survives a restart.
                        // Dynamic NodeIds can be reused by different splices.
                        report
                            .state
                            .prior_firings()
                            .into_iter()
                            .filter(|(node, _)| graph.node(*node).is_some())
                            .collect(),
                        middleware_state,
                    )?;
                }
                self.start_invocation(invocation, running)?;
            }
            EngineExit::Terminal { status } => {
                let result = project_result(execution, status, &graph, &report.state);
                self.refuse_secret(&result)?;
                self.append(CoordinatorEvent::InvocationFinished {
                    invocation,
                    result: result.clone(),
                })?;
                self.release_invocation_leases(invocation, result.status, releasing);
                if invocation == InvocationId::ROOT {
                    self.last_root_report = Some(report);
                }
            }
        }
        Ok(())
    }

    fn check_report(
        execution: ExecutionId,
        report: &driver::ExecutionReport,
    ) -> Result<(), CoordinatorError> {
        if let Some(error) = report.observer_errors.first() {
            return Err(CoordinatorError::EventWriter {
                execution,
                message: error.to_string(),
            });
        }
        Ok(())
    }

    fn declare_first_execution(
        &mut self,
        invocation: InvocationId,
    ) -> Result<ExecutionId, CoordinatorError> {
        let execution = self.store.state().next_execution_id();
        let declaration = &self.store.state().invocations[&invocation].declaration;
        self.append(CoordinatorEvent::ExecutionDeclared {
            execution,
            invocation,
            predecessor: None,
            start: EngineStart {
                entry:           EntryPoint::GraphEntries,
                context:         declaration.context.clone(),
                prior_firings:   BTreeMap::new(),
                execution_index: 0,
                max_executions:  self.options.max_executions,
            },
            middleware_state: initial_middleware_state(&self.middleware),
        })?;
        Ok(execution)
    }

    fn declare_successor(
        &mut self,
        invocation: InvocationId,
        predecessor: ExecutionId,
        target: ir::NodeId,
        prior_firings: BTreeMap<ir::NodeId, u32>,
        middleware_state: MiddlewareState,
    ) -> Result<ExecutionId, CoordinatorError> {
        let count = self.store.state().invocations[&invocation].executions.len();
        if count >= self.options.max_executions as usize {
            return Err(CoordinatorError::ExecutionLimit { invocation });
        }
        let execution = self.store.state().next_execution_id();
        self.append(CoordinatorEvent::ExecutionDeclared {
            execution,
            invocation,
            predecessor: Some(predecessor),
            start: EngineStart {
                entry: EntryPoint::Node(target),
                context: BTreeMap::new(),
                prior_firings,
                execution_index: u32::try_from(count)
                    .expect("the execution limit is represented by u32"),
                max_executions: self.options.max_executions,
            },
            middleware_state,
        })?;
        Ok(execution)
    }

    fn prepare_execution(
        &mut self,
        invocation: InvocationId,
        execution: ExecutionId,
        start: &EngineStart,
        middleware_state: MiddlewareState,
        graph: &Graph,
    ) -> Result<PreparedExecution, CoordinatorError> {
        let mut cancel_before_run = self.store.state().invocations[&invocation].cancelled;
        let directory = self.store.create_execution_dir(invocation, execution)?;
        let events = directory.join(EVENTS_FILE);
        let secrets = self.invocation_secrets(invocation);
        let pipeline = Arc::new(
            MiddlewarePipeline::new(
                invocation,
                execution,
                self.middleware.clone(),
                middleware_state,
            )
            .map_err(|error| CoordinatorError::EventWriter {
                execution,
                message: error.to_string(),
            })?,
        );
        let (driver, writer): (driver::Driver, Arc<JsonlEngineLog>) = if events.exists()
            && events
                .metadata()
                .map_err(|source| CoordinatorError::Io {
                    action: "inspect",
                    path: events.clone(),
                    source,
                })?
                .len()
                > 0
        {
            let decoded = read_engine_log(&events)?;
            // Replaying an existing root cancellation already restores it.
            // Sending it again would ask the driver to escalate to a kill.
            cancel_before_run &= !decoded.log.events().any(|event| {
                matches!(
                    event,
                    Event::CancelRequested { scope } | Event::KillRequested { scope }
                        if *scope == ir::CancelScopeId::ROOT
                )
            });
            if decoded.torn {
                let file = OpenOptions::new()
                    .write(true)
                    .open(&events)
                    .map_err(|source| CoordinatorError::Io {
                        action: "open",
                        path: events.clone(),
                        source,
                    })?;
                file.set_len(decoded.clean_len as u64)
                    .map_err(|source| CoordinatorError::Io {
                        action: "truncate",
                        path: events.clone(),
                        source,
                    })?;
            }
            if !pipeline.is_empty() {
                rebuild_middleware(&pipeline, graph, &decoded.log).map_err(|error| {
                    CoordinatorError::EventWriter {
                        execution,
                        message: error.to_string(),
                    }
                })?;
            }
            let high_water = decoded.log.len() as u64;
            let writer = Arc::new(JsonlEngineLog::append(&events, high_water)?);
            let sandbox = self.prepare_sandbox(invocation, execution, writer.clone())?;
            let (driver, _) = self.runtime.resume_driver(
                (*graph).clone(),
                decoded.log,
                &directory,
                execution.environment_prefix(),
                invocation.workspace_prefix(),
                sandbox,
                secrets,
            )?;
            (driver.with_engine_start(start.clone()), writer)
        } else {
            let writer = Arc::new(JsonlEngineLog::create(&events)?);
            let sandbox = self.prepare_sandbox(invocation, execution, writer.clone())?;
            let driver = self.runtime.driver(
                (*graph).clone(),
                start.clone(),
                &directory,
                execution.environment_prefix(),
                invocation.workspace_prefix(),
                sandbox,
                secrets,
            );
            (driver, writer)
        };
        let client = CoordinatorInvocationClient::new(execution, self.start_tx.clone());
        let fold = Arc::new(pipeline.fold_observer());
        let mut driver = driver
            .observe(writer.clone())
            .observe(fold)
            .with_decision_resolver(pipeline.clone())
            .with_capability(client);
        for observer in &self.observers {
            driver = driver.observe(Arc::new(crate::AddressedObserver::new(
                execution,
                observer.clone(),
            )));
        }
        Ok(PreparedExecution {
            driver,
            pipeline,
            cancel_before_run,
        })
    }

    fn invocation_secrets(&self, invocation: InvocationId) -> Arc<dyn executor::SecretProvider> {
        if invocation == InvocationId::ROOT {
            return self.runtime.secret_provider();
        }
        let declaration = &self.store.state().invocations[&invocation].declaration;
        let call = declaration
            .call
            .as_ref()
            .expect("a non-root invocation has a parent call");
        let parent = self.store.state().executions[&call.parent]
            .declaration
            .invocation;
        Arc::new(InvocationSecrets::new(
            self.invocation_secrets(parent),
            declaration.secret_bindings.clone(),
        ))
    }

    /// Where an execution's scopes run. An isolated invocation owns one
    /// lease per stable scope identity, reserved before any executor sees
    /// it; an inherited one runs every scope in its caller's sandbox — the
    /// caller's workspace, the caller's runtime target, one shared lease.
    fn prepare_sandbox(
        &mut self,
        invocation: InvocationId,
        execution: ExecutionId,
        writer: Arc<JsonlEngineLog>,
    ) -> Result<SandboxAssignment, CoordinatorError> {
        match self.store.state().invocations[&invocation]
            .declaration
            .sandbox
        {
            SandboxBinding::Inherited { lease } => {
                let (workspace, runtime) = {
                    let resources = self.resources();
                    let record = resources
                        .resolve_usable(lease)
                        .map_err(|error| match error {
                            ResourceError::DeletedLease(lease) => {
                                CoordinatorError::InvalidResource { lease }
                            }
                            other => other.into(),
                        })?;
                    (record.workspace.clone(), record.runtime.clone())
                };
                Ok(SandboxAssignment {
                    workspace_override: Some(workspace),
                    runtime_override:   Some(runtime),
                    leases:             ScopeLeases::Shared(lease),
                })
            }
            SandboxBinding::Isolated => {
                let acquired = Arc::new(Mutex::new(BTreeMap::new()));
                self.execution_leases.insert(execution, acquired.clone());
                let allocator = InvocationLeaseAllocator {
                    invocation,
                    execution,
                    resources: self.resources.clone(),
                    acquired,
                    router: self.runtime.sandbox_router().cloned(),
                    writer,
                };
                Ok(SandboxAssignment {
                    workspace_override: None,
                    runtime_override:   None,
                    leases:             ScopeLeases::Owned(Arc::new(allocator)),
                })
            }
        }
    }

    async fn handle_start(
        &mut self,
        request: StartRequest,
    ) -> Result<Option<InvocationId>, CoordinatorError> {
        // A completed driver cannot own a new child. Requests whose callers
        // disappeared can still be queued when that driver's report arrives.
        if request.reply.is_closed() || !self.active_handles.contains_key(&request.parent) {
            let _ = request.reply.send(Err(InvokeError::CoordinatorUnavailable));
            return Ok(None);
        }

        let key = ParentCallKey {
            parent:  request.parent,
            firing:  request.request.site.firing,
            attempt: request.request.site.attempt,
            slot:    request.request.site.slot.clone(),
        };
        match self.resolve_start(&request, &key).await {
            Err(error) => {
                let _ = request.reply.send(Err(error));
                Ok(None)
            }
            Ok(StartOutcome::Requeue { previous }) => {
                // The request re-enters the queue once the superseded attempt
                // settles; the reply travels with it.
                let sender = self
                    .statuses
                    .entry(previous)
                    .or_insert_with(|| watch::channel(InvocationStatus::Declared).0);
                let mut status = sender.subscribe();
                let starts = self.start_tx.clone();
                tokio::spawn(async move {
                    loop {
                        if matches!(*status.borrow(), InvocationStatus::Finished(_)) {
                            let _ = starts.send(request).await;
                            break;
                        }
                        if status.changed().await.is_err() {
                            break;
                        }
                    }
                });
                Ok((!self.active.contains(&previous)).then_some(previous))
            }
            Ok(StartOutcome::Attach { invocation, is_new }) => {
                let sender = self.statuses.entry(invocation).or_insert_with(|| {
                    let status = self.store.state().invocations[&invocation]
                        .result
                        .clone()
                        .map_or(InvocationStatus::Declared, InvocationStatus::Finished);
                    watch::channel(status).0
                });
                let handle =
                    InvocationHandle::new(invocation, sender.subscribe(), self.cancel_tx.clone());
                let reply_delivered = request.reply.send(Ok(handle)).is_ok();
                let incomplete = self.store.state().invocations[&invocation].result.is_none();
                let parent = self.store.state().executions[&request.parent]
                    .declaration
                    .invocation;
                if incomplete
                    && !self.store.state().invocations[&invocation].cancelled
                    && (!reply_delivered || self.store.state().invocations[&parent].cancelled)
                {
                    self.cancel_invocations(vec![(invocation, false)]).await?;
                }
                Ok((incomplete
                    && !self.active.contains(&invocation)
                    && (is_new || reply_delivered))
                    .then_some(invocation))
            }
        }
    }

    /// Decide what a start request attaches to. Every rejection comes back as
    /// the error; `handle_start` owns the one reply send.
    async fn resolve_start(
        &mut self,
        request: &StartRequest,
        key: &ParentCallKey,
    ) -> Result<StartOutcome, InvokeError> {
        if !self.store.state().graphs.contains(&request.request.graph) {
            return Err(InvokeError::UnknownGraph(request.request.graph));
        }
        self.refuse_secret(&request.request.context)
            .map_err(invoke_error)?;

        if let Some(invocation) = self.store.state().calls.get(key).copied() {
            let declaration = &self.store.state().invocations[&invocation].declaration;
            // A kind-match only: when the parent invocation is itself
            // inherited, the child reuses the parent's lease, whose scope
            // belongs to an ancestor's graph rather than the caller's.
            let sandbox_matches = matches!(
                (request.request.sandbox, declaration.sandbox),
                (SandboxMode::Isolated, SandboxBinding::Isolated)
                    | (
                        SandboxMode::Inherit { .. },
                        SandboxBinding::Inherited { .. }
                    )
            );
            if declaration.graph != request.request.graph
                || declaration.context != request.request.context
                || declaration.secret_bindings != request.request.secrets
                || !sandbox_matches
            {
                return Err(InvokeError::RequestMismatch);
            }
            return Ok(StartOutcome::Attach {
                invocation,
                is_new: false,
            });
        }

        let previous = self
            .store
            .state()
            .calls
            .iter()
            .filter(|(candidate, _)| {
                candidate.parent == key.parent
                    && candidate.firing == key.firing
                    && candidate.slot == key.slot
            })
            .max_by_key(|(candidate, _)| candidate.attempt)
            .map(|(candidate, invocation)| (candidate.clone(), *invocation));
        if let Some((previous_key, previous)) = previous {
            if previous_key.attempt > key.attempt {
                return Err(InvokeError::RequestMismatch);
            }
            if self.store.state().invocations[&previous].result.is_none() {
                self.handle_cancel(previous).await.map_err(invoke_error)?;
                return Ok(StartOutcome::Requeue { previous });
            }
        }
        if self.store.state().invocations.len() >= self.options.max_invocations as usize {
            return Err(InvokeError::InvocationLimit);
        }
        let invocation = self.store.state().next_invocation_id();
        let sandbox = match request.request.sandbox {
            SandboxMode::Isolated => SandboxBinding::Isolated,
            SandboxMode::Inherit { scope } => match self.inherited_binding(key, scope) {
                Ok(binding) => binding,
                Err(CoordinatorError::NoInheritableSandbox) => {
                    return Err(InvokeError::NoInheritableSandbox);
                }
                Err(error) => return Err(invoke_error(error)),
            },
        };
        if let SandboxBinding::Inherited { lease } = sandbox {
            self.check_inherited_container(lease, request.request.graph)?;
        }
        self.append(CoordinatorEvent::InvocationDeclared {
            invocation,
            call: Some(key.clone()),
            graph: request.request.graph,
            context: request.request.context.clone(),
            secret_bindings: request.request.secrets.clone(),
            sandbox,
        })
        .map_err(invoke_error)?;
        Ok(StartOutcome::Attach {
            invocation,
            is_new: true,
        })
    }

    async fn handle_cancel(&mut self, cancelled: InvocationId) -> Result<(), CoordinatorError> {
        if !self.store.state().invocations.contains_key(&cancelled) {
            return Ok(());
        }

        let affected: Vec<_> = self
            .store
            .state()
            .invocations
            .iter()
            .filter_map(|(candidate, state)| {
                (state.result.is_none() && self.is_descendant_or_same(*candidate, cancelled))
                    .then_some((*candidate, state.cancelled))
            })
            .collect();
        self.cancel_invocations(affected).await
    }

    async fn cancel_invocations(
        &mut self,
        affected: Vec<(InvocationId, bool)>,
    ) -> Result<(), CoordinatorError> {
        for (invocation, already_cancelled) in &affected {
            if !already_cancelled {
                self.append(CoordinatorEvent::InvocationCancelRequested {
                    invocation: *invocation,
                })?;
            }
        }

        let affected: BTreeSet<_> = affected
            .into_iter()
            .map(|(invocation, _)| invocation)
            .collect();
        let handles: Vec<_> = self
            .active_handles
            .values()
            .filter(|(invocation, _)| affected.contains(invocation))
            .map(|(_, handle)| handle.clone())
            .collect();
        for handle in handles {
            handle.cancel(ir::CancelScopeId::ROOT).await;
        }
        Ok(())
    }

    fn handle_control(&self, request: ControlRequest) {
        let Some((_, handle)) = self.active_handles.get(&request.execution) else {
            let _ = request.reply.send(driver::DeliverDisposition::NotLive);
            return;
        };
        let handle = handle.clone();
        tokio::spawn(async move {
            let disposition = handle.deliver(request.firing, request.control).await;
            let _ = request.reply.send(disposition);
        });
    }

    fn is_descendant_or_same(&self, mut invocation: InvocationId, ancestor: InvocationId) -> bool {
        loop {
            if invocation == ancestor {
                return true;
            }
            let Some(call) = self.store.state().invocations[&invocation]
                .declaration
                .call
                .as_ref()
            else {
                return false;
            };
            let Some(parent) = self.store.state().executions.get(&call.parent) else {
                return false;
            };
            invocation = parent.declaration.invocation;
        }
    }

    /// Resolve `SandboxMode::Inherit` for a new child. The call key pins the
    /// parent invocation, and the caller names its own scope, so no engine log
    /// is read. The driver registers every acquired scope, including dynamic
    /// ones, before a step can invoke a child.
    fn inherited_binding(
        &mut self,
        call: &ParentCallKey,
        scope: ir::ScopeId,
    ) -> Result<SandboxBinding, CoordinatorError> {
        let execution = self
            .store
            .state()
            .executions
            .get(&call.parent)
            .ok_or(CoordinatorError::NoInheritableSandbox)?;
        let parent_invocation = execution.declaration.invocation;
        if let SandboxBinding::Inherited { lease } = self.store.state().invocations
            [&parent_invocation]
            .declaration
            .sandbox
        {
            self.resources().resolve_usable(lease)?;
            return Ok(SandboxBinding::Inherited { lease });
        }
        let lease = self
            .execution_leases
            .get(&call.parent)
            .and_then(|leases| {
                leases
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&scope)
                    .copied()
            })
            .ok_or(CoordinatorError::NoInheritableSandbox)?;
        self.resources().resolve_usable(lease)?;
        Ok(SandboxBinding::Inherited { lease })
    }

    /// An inherited child runs in its caller's sandbox, so a container it
    /// declares must be the caller's own: absent (the scope takes the
    /// caller's target) or equal. A different image or option set is a
    /// deterministic refusal at declaration; a child that needs its own
    /// image uses an isolated binding.
    fn check_inherited_container(
        &mut self,
        lease: crate::SandboxLeaseId,
        child: GraphDigest,
    ) -> Result<(), InvokeError> {
        let parent = self
            .resources()
            .resolve(lease)
            .map(|record| record.runtime.clone())
            .map_err(invoke_error)?;
        let graph = self.store.load_graph(child).map_err(invoke_error)?;
        for scope in &graph.scopes {
            let declares_container =
                matches!(scope.runtime.target, RuntimeTarget::Container { .. });
            if declares_container && scope.runtime.target != parent.target {
                return Err(InvokeError::InheritedContainerMismatch { scope: scope.id });
            }
        }
        Ok(())
    }

    fn append(&mut self, event: CoordinatorEvent) -> Result<CoordinatorRecord, CoordinatorError> {
        let record = self.store.append(event)?;
        for observer in &self.observers {
            observer.on_lifecycle(&record);
        }
        Ok(record)
    }

    fn refuse_secret<T: serde::Serialize>(&self, value: &T) -> Result<(), CoordinatorError> {
        let encoded = serde_json::to_string(value).map_err(CoordinatorError::EncodeDurableData)?;
        self.refuse_secret_bytes(&encoded)
    }

    fn refuse_secret_bytes(&self, encoded: &str) -> Result<(), CoordinatorError> {
        if self.runtime.masker().contains_secret(encoded) {
            return Err(CoordinatorError::SecretInDurableData);
        }
        Ok(())
    }
}

fn validate_resources(
    store: &mut CoordinatorStore,
    resources: &ResourceStore,
) -> Result<(), CoordinatorError> {
    let mut replayed = BTreeMap::new();
    for record in resources.records() {
        let invalid = || CoordinatorError::InvalidResource {
            lease: record.lease,
        };
        let Some(invocation) = store.state().invocations.get(&record.allocation.invocation) else {
            return Err(CoordinatorError::InvalidResource {
                lease: record.lease,
            });
        };
        if invocation.declaration.sandbox != SandboxBinding::Isolated {
            return Err(CoordinatorError::InvalidResource {
                lease: record.lease,
            });
        }
        let digest = invocation.declaration.graph;
        let graph = store.load_graph(digest)?;
        match &record.allocation.scope {
            engine::ScopeIdentity::Declared(scope) => {
                if record.introduced_by.is_some()
                    || graph
                        .scope(*scope)
                        .is_none_or(|scope| scope.runtime != record.runtime)
                {
                    return Err(invalid());
                }
            }
            identity @ engine::ScopeIdentity::Spliced(_) => {
                let execution = record.introduced_by.ok_or_else(invalid)?;
                if store
                    .state()
                    .executions
                    .get(&execution)
                    .is_none_or(|execution| {
                        execution.declaration.invocation != record.allocation.invocation
                    })
                {
                    return Err(invalid());
                }
                if let Entry::Vacant(entry) = replayed.entry(execution) {
                    let events = store
                        .execution_dir(record.allocation.invocation, execution)
                        .join(EVENTS_FILE);
                    let log = read_engine_log(&events)?;
                    let point =
                        engine::resume((*graph).clone(), &log.log).map_err(|_| invalid())?;
                    entry.insert(point.state);
                }
                let state = &replayed[&execution];
                if !state.graph().scopes.iter().any(|scope| {
                    state.scope_identity(scope.id).as_ref() == Some(identity)
                        && scope.runtime == record.runtime
                }) {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(())
}

fn project_result(
    execution: ExecutionId,
    mut status: RunStatus,
    graph: &Graph,
    state: &engine::EngineState,
) -> InvocationResult {
    let mut failure = state
        .history()
        .iter()
        .rev()
        .find_map(|record| record.outcome.status.failure_info().cloned());
    if failure.is_none() && status == RunStatus::Failed {
        failure =
            Some(FailureInfo::new(state.errors().last().map_or_else(
                || "execution failed".to_string(),
                ToString::to_string,
            )));
    }
    let output = match graph.result {
        ResultProjection::None => Value::Null,
        ResultProjection::NodeOutput(node) => graph
            .node(node)
            .and_then(|node| state.run_context().node(&node.name))
            .map_or_else(
                || {
                    if status == RunStatus::Success {
                        status = RunStatus::Failed;
                        failure = Some(
                            FailureInfo::new("the declared invocation result node did not run")
                                .with_class(FailureClass::new_static("invalid_invocation_result")),
                        );
                    }
                    Value::Null
                },
                |record| record.output.clone(),
            ),
    };
    InvocationResult {
        status,
        failure,
        final_execution: execution,
        output,
        context: (*state.run_context().kv).clone(),
    }
}

fn rebuild_middleware(
    pipeline: &MiddlewarePipeline,
    graph: &Graph,
    log: &engine::EventLog,
) -> Result<(), crate::MiddlewareError> {
    let state = engine::replay(graph.clone(), log);
    let final_attempts: BTreeSet<_> = state
        .history()
        .iter()
        .map(|record| (record.firing, record.attempt))
        .collect();
    let nodes_by_firing: BTreeMap<_, _> = state
        .history()
        .iter()
        .map(|record| (record.firing, record.node))
        .collect();
    // Fold the durable prefix here. `Driver::resume` presents any regenerated
    // core suffix to the fold observer before it dispatches pending commands,
    // so folding that suffix here too would count it twice. The derivation is
    // the live observer's own, so the two paths cannot drift.
    for record in log.records() {
        let fold = derive_fold_event(
            &record.event,
            |firing, attempt| final_attempts.contains(&(firing, attempt)),
            |firing| nodes_by_firing.get(&firing).copied(),
            |edge| {
                graph
                    .edge(edge)
                    .is_some_and(|edge| edge.transition == ir::EdgeTransition::Restart)
            },
        );
        if let Some(event) = fold {
            pipeline.fold(&event)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;
