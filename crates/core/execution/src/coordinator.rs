use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::{fmt, fs, io};

use driver::{
    ExecutionSlot, HookContext, ParentLink, SandboxAssignment, ScopeLease, ScopeLeaseAllocator,
    ScopeLeases,
};
use engine::{EngineExit, EngineStart, EntryPoint, Event, MiddlewareKey};
use executor_sandbox::{CONTAINER_KIND, RecordedLease, RoutingExecutor};
use ir::{
    Control, FailureClass, FailureInfo, FiringId, Graph, ResultProjection, RunStatus,
    RuntimeTarget, ScopeId, Value,
};
use runtime::{RunAccess, RunRuntime};
use smol_str::SmolStr;
use store::{RunLogs, execution_relative_dir};
use tokio::sync::{
    Mutex as AsyncMutex, MutexGuard, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch,
};
use tokio::task::{AbortHandle, JoinError, JoinSet};

use crate::client::StartRequest;
use crate::middleware::derive_fold_event;
use crate::{
    CancelReason, CancelRequest, CoordinatorEvent, CoordinatorInvocationClient, CoordinatorRecord,
    CoordinatorStore, EngineLogError, ExecutionId, ExecutionLogWriter, ExecutionObserver,
    GraphDigest, HOST_PROVIDER, InvocationHandle, InvocationId, InvocationResult,
    InvocationSecrets, InvocationStatus, InvokeError, Middleware, MiddlewarePipeline,
    MiddlewareState, ParentCallKey, ResourceError, ResourceLedger, ResourceStore,
    SandboxAllocationKey, SandboxBinding, SandboxMode, SecretBindings, StoreError, StoreWriter,
    initial_middleware_state, read_execution_log,
};

pub const DEFAULT_MAX_INVOCATIONS: u32 = 1024;

/// The hard ceiling on invocations in one run: the root plus every nested
/// and branch invocation, finished, failed and cancelled ones included. No
/// option raises it and none disables it; a lower limit is allowed.
pub const MAX_INVOCATIONS: u32 = 10_000;

/// A requested invocation limit the coordinator refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvocationLimitError {
    #[error("the invocation limit cannot be disabled; it must be at least 1")]
    Disabled,
    #[error("the invocation limit {requested} is above the hard ceiling of {ceiling}")]
    AboveCeiling { requested: u32, ceiling: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoordinatorOptions {
    pub max_invocations: u32,
    pub max_executions:  u32,
}

impl CoordinatorOptions {
    /// Lower the invocation limit. Zero (no limit) and anything above
    /// [`MAX_INVOCATIONS`] are refused.
    pub fn with_max_invocations(mut self, limit: u32) -> Result<Self, InvocationLimitError> {
        Self::check_limit(limit)?;
        self.max_invocations = limit;
        Ok(self)
    }

    fn check_limit(limit: u32) -> Result<(), InvocationLimitError> {
        if limit == 0 {
            return Err(InvocationLimitError::Disabled);
        }
        if limit > MAX_INVOCATIONS {
            return Err(InvocationLimitError::AboveCeiling {
                requested: limit,
                ceiling:   MAX_INVOCATIONS,
            });
        }
        Ok(())
    }
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
    /// The run could not be opened in its store.
    #[error(transparent)]
    Open(#[from] store::StoreError),
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
    #[error(
        "the run declares {total} invocations, above its limit of {limit}; the limit cannot be \
         lowered below what the run already holds"
    )]
    InvocationLimit { total: u64, limit: u32 },
    #[error(transparent)]
    InvalidInvocationLimit(#[from] InvocationLimitError),
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
    /// The fork slot a gated child holds while it is live, released once
    /// its `ExecutionFinished` is recorded.
    slot:              Option<ExecutionSlot>,
}

/// A driver has stopped; its invocation can finish after its descendants
/// settle.
struct CompletedExecution {
    invocation:       InvocationId,
    execution:        ExecutionId,
    graph:            Arc<Graph>,
    report:           driver::ExecutionReport,
    middleware_state: MiddlewareState,
    slot:             Option<ExecutionSlot>,
}

/// One lease's release: what the run asked for and what the executor did.
struct LeaseRelease {
    lease:   crate::SandboxLeaseId,
    outcome: executor::ScopeOutcome,
    report:  executor::ReleaseReport,
}

/// A finished invocation's leases, released.
struct InvocationReleased {
    invocation: InvocationId,
    releases:   Vec<LeaseRelease>,
}

/// The scope outcome an invocation's status maps to, which decides the
/// retention of its sandboxes.
fn scope_outcome(status: RunStatus) -> executor::ScopeOutcome {
    if status == RunStatus::Success {
        executor::ScopeOutcome::Succeeded
    } else {
        executor::ScopeOutcome::Failed
    }
}

fn log_release_problems(lease: crate::SandboxLeaseId, report: &executor::ReleaseReport) {
    for problem in &report.problems {
        tracing::warn!(
            lease = lease.raw(),
            problem,
            "sandbox lease release problem"
        );
    }
}

type ExecutionLeases = Arc<Mutex<BTreeMap<ScopeId, crate::SandboxLeaseId>>>;

/// The coordinator delegates reservation to each execution's acquire tasks.
/// The shared resource store still serializes lease ID allocation and writes.
struct InvocationLeaseAllocator {
    invocation: InvocationId,
    execution:  ExecutionId,
    resources:  Arc<AsyncMutex<ResourceStore>>,
    acquired:   ExecutionLeases,
    router:     Option<Arc<RoutingExecutor>>,
    writer:     Arc<ExecutionLogWriter>,
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
        // This queues a marker behind all records already observed by the
        // driver.
        if introduced_by.is_some() {
            self.writer.flush().await.map_err(|source| error(&source))?;
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
        let runtime = spec.runtime.clone();
        let assignment = {
            let mut resources = self.resources.lock().await;
            let record = resources
                .reserve_scope(allocation, &provider, runtime, introduced_by)
                .await
                .map_err(|source| error(&source))?;
            if record.state == crate::LeaseState::Deleted {
                return Err(error(&ResourceError::DeletedLease(record.lease)));
            }
            ScopeLease {
                lease:     record.lease,
                workspace: record.workspace.clone(),
                provider:  record.provider.clone(),
            }
        };
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

/// A request to record a run-level pause or unpause. The reply fires once
/// the record is durable, or at once when the state already says so.
struct PauseRequest {
    paused: bool,
    reply:  oneshot::Sender<()>,
}

/// A cloneable control path for a coordinator that is currently running.
#[derive(Clone)]
pub struct CoordinatorHandle {
    cancel:  mpsc::UnboundedSender<CancelRequest>,
    control: mpsc::UnboundedSender<ControlRequest>,
    pause:   mpsc::UnboundedSender<PauseRequest>,
}

impl CoordinatorHandle {
    /// Politely cancel an invocation and every active descendant. A second
    /// request on an invocation that is already cancelled reaches its
    /// drivers again, which escalates them to the kill tier.
    pub fn cancel(&self, invocation: InvocationId) {
        let _ = self.cancel.send(CancelRequest {
            invocation,
            reason: None,
            escalate: true,
        });
    }

    /// Politely cancel the complete root run.
    pub fn cancel_root(&self) {
        self.cancel(InvocationId::ROOT);
    }

    /// Politely cancel the complete root run and record why: the reason
    /// rides the `InvocationCancelRequested` record and the public event.
    pub fn cancel_root_for(&self, reason: CancelReason) {
        let _ = self.cancel.send(CancelRequest {
            invocation: InvocationId::ROOT,
            reason:     Some(reason),
            escalate:   true,
        });
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

    /// Record a run-level pause (`true`) or unpause (`false`) durably. The
    /// returned future completes once the record is on disk and every
    /// observer has seen it, so a caller that releases admission afterwards
    /// never releases before the unpause is durable. A redundant request
    /// (the state already says so) records nothing. Completes at once when
    /// the coordinator is gone: nothing is left to record against.
    pub async fn set_paused(&self, paused: bool) {
        let (reply, recorded) = oneshot::channel();
        if self.pause.send(PauseRequest { paused, reply }).is_err() {
            return;
        }
        let _ = recorded.await;
    }
}

/// A fork gate's identity: the parent execution and the gate name its
/// children were declared under.
type GateKey = (ExecutionId, SmolStr);

/// One fork's admission: the slots its children share, and the declared
/// children waiting for a slot before their driver starts, in declaration
/// order. A child holds its slot from dispatch to the end of its driver,
/// except across a retry backoff, so a fork never has more live children
/// than slots, plus the children waiting out a backoff.
struct ForkGate {
    slots:  Arc<Semaphore>,
    queued: VecDeque<InvocationId>,
    /// The task waiting for the next free slot while `queued` is not empty.
    waiter: Option<AbortHandle>,
}

impl ForkGate {
    fn new(max_parallel: u32) -> Self {
        let limit = usize::try_from(max_parallel.max(1)).unwrap_or(usize::MAX);
        Self {
            slots:  Arc::new(Semaphore::new(limit)),
            queued: VecDeque::new(),
            waiter: None,
        }
    }

    /// Wait for the next free slot while children are queued; at most one
    /// wait at a time. The slot comes back on the coordinator's admission
    /// channel, for the child at the head of the queue.
    fn wait_for_slot(&mut self, key: GateKey, admit: mpsc::UnboundedSender<AdmittedSlot>) {
        if self.waiter.is_some() || self.queued.is_empty() {
            return;
        }
        let slots = self.slots.clone();
        let waiter = tokio::spawn(async move {
            let Ok(permit) = slots.acquire_owned().await else {
                return;
            };
            let _ = admit.send(AdmittedSlot { gate: key, permit });
        });
        self.waiter = Some(waiter.abort_handle());
    }
}

impl Drop for ForkGate {
    fn drop(&mut self) {
        if let Some(waiter) = &self.waiter {
            waiter.abort();
        }
    }
}

/// A free slot of one fork gate, taken for the next queued child.
struct AdmittedSlot {
    gate:   GateKey,
    permit: OwnedSemaphorePermit,
}

/// Owns one run's lifecycle log, engines, resource boundary, and observers.
pub struct Coordinator {
    store:            CoordinatorStore,
    /// The run's store writer: every execution's records go through it.
    writer:           Arc<StoreWriter>,
    runtime:          RunRuntime,
    options:          CoordinatorOptions,
    observers:        Vec<Arc<dyn ExecutionObserver>>,
    start_tx:         mpsc::Sender<StartRequest>,
    start_rx:         mpsc::Receiver<StartRequest>,
    cancel_tx:        mpsc::UnboundedSender<CancelRequest>,
    cancel_rx:        mpsc::UnboundedReceiver<CancelRequest>,
    control_tx:       mpsc::UnboundedSender<ControlRequest>,
    control_rx:       mpsc::UnboundedReceiver<ControlRequest>,
    pause_tx:         mpsc::UnboundedSender<PauseRequest>,
    pause_rx:         mpsc::UnboundedReceiver<PauseRequest>,
    admit_tx:         mpsc::UnboundedSender<AdmittedSlot>,
    admit_rx:         mpsc::UnboundedReceiver<AdmittedSlot>,
    statuses:         BTreeMap<InvocationId, watch::Sender<InvocationStatus>>,
    active:           BTreeSet<InvocationId>,
    active_handles:   BTreeMap<ExecutionId, (InvocationId, driver::RunHandle)>,
    middleware:       Vec<Arc<dyn Middleware>>,
    last_root_report: Option<driver::ExecutionReport>,
    /// The durable lease records, shared with the executor's lease manager
    /// as its ledger.
    resources:        Arc<AsyncMutex<ResourceStore>>,
    execution_leases: BTreeMap<ExecutionId, ExecutionLeases>,
    /// Fork gates by parent execution and gate name: the slots every child
    /// invocation declared under that name shares, and the children queued
    /// for one. Rebuilt on demand, so a resume recovers the accounting from
    /// the children it redispatches.
    gates:            BTreeMap<GateKey, ForkGate>,
    /// Whether this coordinator continues a stored run: an observer that
    /// attaches then is handed the replayed state first.
    resumed:          bool,
    #[cfg(test)]
    release_gate:     Option<Arc<tests::ReleaseGate>>,
}

impl Coordinator {
    /// Start a fresh run. `middleware` may be empty; the configured chain's
    /// keys are recorded durably either way.
    pub async fn create(
        runtime: RunRuntime,
        middleware: Vec<Arc<dyn Middleware>>,
        options: CoordinatorOptions,
    ) -> Result<Self, CoordinatorError> {
        CoordinatorOptions::check_limit(options.max_invocations)?;
        let keys = middleware.iter().map(|item| item.key()).collect();
        let logs = runtime.open(RunAccess::Create).await?;
        let store = CoordinatorStore::create(logs.clone(), runtime.run_key().clone(), keys).await?;
        let resources = ResourceStore::load(&logs).await?;
        Ok(Self::assemble(
            store, resources, runtime, middleware, options, false,
        ))
    }

    /// Resume a crashed run. `middleware` must match the recorded chain.
    pub async fn resume(
        runtime: RunRuntime,
        middleware: Vec<Arc<dyn Middleware>>,
        options: CoordinatorOptions,
    ) -> Result<Self, CoordinatorError> {
        CoordinatorOptions::check_limit(options.max_invocations)?;
        let logs = runtime.open(RunAccess::Write).await?;
        let mut store = CoordinatorStore::resume(logs.clone(), runtime.run_key().clone()).await?;
        let resources = ResourceStore::load(&logs).await?;
        let keys: Vec<MiddlewareKey> = middleware.iter().map(|item| item.key()).collect();
        if store.state().middleware_chain != keys {
            return Err(StoreError::State(crate::StateError::MiddlewareChain).into());
        }
        let total = store.state().invocations.len() as u64;
        if total > u64::from(options.max_invocations) {
            return Err(CoordinatorError::InvocationLimit {
                total,
                limit: options.max_invocations,
            });
        }
        validate_resources(&mut store, &resources).await?;
        // A finished invocation is never dispatched again, so its inherited
        // lease need not be in this run's ledger: a forked run carries the
        // source's finished children, whose leases were the source's
        // (`FORK.md`).
        for lease in store
            .state()
            .invocations
            .values()
            .filter(|invocation| invocation.result.is_none())
            .filter_map(|invocation| match invocation.declaration.sandbox {
                SandboxBinding::Inherited { lease } => Some(lease),
                SandboxBinding::Isolated => None,
            })
        {
            resources.resolve(lease)?;
        }
        let coordinator = Self::assemble(store, resources, runtime, middleware, options, true);
        coordinator.reconcile_leases().await?;
        Ok(coordinator)
    }

    /// The takeover step of a resume: before any create, adopt what a lost
    /// create left on a provider and remove what no record names. Runs
    /// with the ledger attached, so an adopted lease is recorded live.
    async fn reconcile_leases(&self) -> Result<(), CoordinatorError> {
        let (host, container): (Vec<_>, Vec<_>) = self
            .resources()
            .await
            .records()
            .filter(|record| record.state != crate::LeaseState::Deleted)
            .map(|record| {
                (record.provider == HOST_PROVIDER, RecordedLease {
                    lease:        record.lease,
                    workspace_id: record.workspace.as_str().to_owned(),
                })
            })
            .partition::<Vec<_>, _>(|(host, _)| *host);
        let host: Vec<RecordedLease> = host.into_iter().map(|(_, lease)| lease).collect();
        let container: Vec<RecordedLease> = container.into_iter().map(|(_, lease)| lease).collect();
        if host.is_empty() && container.is_empty() {
            return Ok(());
        }
        let report = self.runtime.reconcile_leases(&host, &container).await;
        for (lease, sandbox) in &report.adopted {
            tracing::info!(lease = lease.raw(), sandbox = %sandbox, "adopted a lost create");
        }
        for sandbox in &report.removed {
            tracing::warn!(sandbox = %sandbox, "removed a sandbox no lease record names");
        }
        for problem in &report.problems {
            tracing::warn!(problem, "sandbox lease reconciliation problem");
        }
        Ok(())
    }

    fn assemble(
        store: CoordinatorStore,
        resources: ResourceStore,
        runtime: RunRuntime,
        middleware: Vec<Arc<dyn Middleware>>,
        options: CoordinatorOptions,
        resumed: bool,
    ) -> Self {
        let (start_tx, start_rx) = mpsc::channel(128);
        let (cancel_tx, cancel_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (pause_tx, pause_rx) = mpsc::unbounded_channel();
        let (admit_tx, admit_rx) = mpsc::unbounded_channel();
        // The records are the executor's ledger from here on: every
        // container scope it allocates is written here before it exists.
        let resources = Arc::new(AsyncMutex::new(resources));
        runtime.attach_lease_ledger(Arc::new(ResourceLedger::new(resources.clone())));
        let writer = StoreWriter::start(store.logs());
        Self {
            store,
            writer,
            runtime,
            options,
            observers: Vec::new(),
            start_tx,
            start_rx,
            cancel_tx,
            cancel_rx,
            control_tx,
            control_rx,
            pause_tx,
            pause_rx,
            admit_tx,
            admit_rx,
            statuses: BTreeMap::new(),
            active: BTreeSet::new(),
            active_handles: BTreeMap::new(),
            middleware,
            last_root_report: None,
            resources,
            execution_leases: BTreeMap::new(),
            gates: BTreeMap::new(),
            resumed,
            #[cfg(test)]
            release_gate: None,
        }
    }

    /// The invocation limit this run enforces.
    pub fn max_invocations(&self) -> u32 {
        self.options.max_invocations
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
        if self.resumed {
            observer.on_resumed(self.store.state());
        }
        self.observers.push(observer);
        self
    }

    pub fn store(&self) -> &CoordinatorStore {
        &self.store
    }

    /// A registered graph, decoded and validated once and cached by digest.
    pub async fn load_graph(
        &mut self,
        digest: GraphDigest,
    ) -> Result<Arc<Graph>, CoordinatorError> {
        Ok(self.store.load_graph(digest).await?)
    }

    /// The directory an execution's own files live under: its step output
    /// and, in the run-directory store, its engine log.
    pub fn execution_dir(&self, execution: ExecutionId) -> PathBuf {
        self.runtime
            .run_dir()
            .join(execution_relative_dir(execution))
    }

    pub fn take_root_report(&mut self) -> Option<driver::ExecutionReport> {
        self.last_root_report.take()
    }

    pub fn handle(&self) -> CoordinatorHandle {
        CoordinatorHandle {
            cancel:  self.cancel_tx.clone(),
            control: self.control_tx.clone(),
            pause:   self.pause_tx.clone(),
        }
    }

    pub async fn register_graph(&mut self, graph: &Graph) -> Result<GraphDigest, CoordinatorError> {
        let encoded = serde_json::to_string(graph).map_err(CoordinatorError::EncodeDurableData)?;
        self.refuse_secret_bytes(&encoded)?;
        let (digest, record) = self.store.register_graph_bytes(encoded.as_bytes()).await?;
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
                admission: None,
            })
            .await?;
        }

        if let Some(result) = self.store.state().invocations[&InvocationId::ROOT]
            .result
            .clone()
        {
            let execution = result.final_execution;
            let declaration = self.store.state().executions[&execution]
                .declaration
                .clone();
            let registered = self.store.load_graph(graph).await?;
            let recorded = self.store.state().executions[&execution]
                .exit
                .clone()
                .ok_or(CoordinatorError::MissingExit(execution))?;
            let PreparedExecution { driver, .. } = self
                .prepare_execution(
                    InvocationId::ROOT,
                    execution,
                    &declaration.start,
                    declaration.middleware_state,
                    &registered,
                    None,
                )
                .await?;
            let report = driver.run().await;
            Self::check_report(execution, &report)?;
            self.append_run_notes(execution, &report).await?;
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
            })
            .await?;
        }
        Ok(result)
    }

    /// End the run: release every lease still holding a sandbox — an
    /// invocation that finished before a crash, or one that never finished
    /// — with the run's own status, tear the run services down and stop the
    /// store writer. The run's store handle comes back, still holding the
    /// lease, so the caller can read the finished run through it.
    pub async fn finish(mut self) -> Arc<dyn RunLogs> {
        let status = self
            .store
            .state()
            .run_status
            .unwrap_or(RunStatus::Cancelled);
        let remaining: Vec<_> = self
            .resources()
            .await
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
            let release = self.release_lease(lease, owner_status).await;
            let invocation = self
                .resources()
                .await
                .resolve(lease)
                .map(|record| record.allocation.invocation)
                .ok();
            if let Some(invocation) = invocation
                && let Err(error) = self.append_scope_released(invocation, release).await
            {
                tracing::warn!(%error, lease = lease.raw(), "the scope's release was not recorded");
            }
        }
        self.runtime.finish().await;
        if let Err(error) = self.writer.shutdown().await {
            tracing::warn!(%error, "the store writer stopped with an error");
        }
        self.store.logs().clone()
    }

    async fn resources(&self) -> MutexGuard<'_, ResourceStore> {
        self.resources.lock().await
    }

    /// Stop a lease's sandbox, then keep or delete it by retention for the
    /// outcome `status` maps to. Release is best effort; a problem is
    /// logged, and the record keeps its pending intent for the next attempt
    /// (`finish`, or `petri sandbox prune`).
    async fn release_lease(&self, lease: crate::SandboxLeaseId, status: RunStatus) -> LeaseRelease {
        let outcome = scope_outcome(status);
        let report = self.runtime.release_lease(lease, outcome).await;
        log_release_problems(lease, &report);
        LeaseRelease {
            lease,
            outcome,
            report,
        }
    }

    /// Record a lease's release as the run's `scope.released`: the lease's
    /// record after the release says whether its sandbox is still there.
    async fn append_scope_released(
        &mut self,
        invocation: InvocationId,
        release: LeaseRelease,
    ) -> Result<(), CoordinatorError> {
        let LeaseRelease {
            lease,
            outcome,
            report,
        } = release;
        let record = {
            let resources = self.resources().await;
            let Ok(record) = resources.resolve(lease) else {
                return Ok(());
            };
            record.clone()
        };
        self.append(CoordinatorEvent::ScopeReleased {
            invocation,
            lease,
            scope: record.allocation.scope,
            workspace: record.workspace,
            provider: record.provider,
            instance: record.resource_id,
            outcome,
            retained: record.state != crate::LeaseState::Deleted,
            problems: report.problems,
        })
        .await?;
        Ok(())
    }

    /// Release the leases `invocation` allocated, now that it has finished.
    /// An inherited invocation allocated none: its caller's lease outlives
    /// it.
    async fn release_invocation_leases(
        &mut self,
        invocation: InvocationId,
        status: RunStatus,
        releasing: &mut JoinSet<InvocationReleased>,
    ) {
        let owned: Vec<crate::SandboxLeaseId> = self
            .resources()
            .await
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
            let mut releases = Vec::new();
            if let Some(router) = router {
                let outcome = scope_outcome(status);
                for lease in owned {
                    let report = router.release_lease(lease, outcome).await;
                    log_release_problems(lease, &report);
                    releases.push(LeaseRelease {
                        lease,
                        outcome,
                        report,
                    });
                }
            }
            InvocationReleased {
                invocation,
                releases,
            }
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
        releasing: &mut JoinSet<InvocationReleased>,
    ) -> Result<InvocationResult, CoordinatorError> {
        let mut completed = BTreeMap::new();
        if self.store.state().invocations[&InvocationId::ROOT]
            .result
            .is_some()
        {
            self.settle_descendants(InvocationId::ROOT, running).await?;
        } else {
            self.start_invocation(InvocationId::ROOT, running).await?;
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
                    self.append_run_notes(done.execution, &done.report).await?;
                    self.settle_descendants(done.invocation, running).await?;
                    self.drop_idle_gates();
                    completed.insert(done.invocation, done);
                }
                admitted = self.admit_rx.recv() => {
                    if let Some(admitted) = admitted {
                        self.dispatch_admitted(admitted, running).await?;
                    }
                }
                result = releasing.join_next(), if !releasing.is_empty() => {
                    let InvocationReleased { invocation, releases } =
                        result.expect("the release set is not empty")?;
                    for release in releases {
                        self.append_scope_released(invocation, release).await?;
                    }
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
                        self.start_invocation(invocation, running).await?;
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
                request = self.pause_rx.recv() => {
                    if let Some(request) = request {
                        self.handle_pause(request).await?;
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
                self.complete_execution(done, running, releasing).await?;
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
        self.cancel_invocations(uncancelled, InvocationId::ROOT, None, false)
            .await?;
        for (descendant, _) in descendants {
            self.start_invocation(descendant, running).await?;
        }
        Ok(())
    }

    /// Start an unfinished invocation that is not already live. A fork's
    /// child (one declared under a gate) is queued for a slot instead, so the
    /// fork has at most `max_parallel` live children; its declaration is
    /// durable already, and only its driver waits.
    async fn start_invocation(
        &mut self,
        invocation: InvocationId,
        running: &mut JoinSet<CompletedExecution>,
    ) -> Result<(), CoordinatorError> {
        let state = &self.store.state().invocations[&invocation];
        if state.result.is_some() || self.active.contains(&invocation) {
            return Ok(());
        }
        if let Some((key, max_parallel)) = self.fork_admission(invocation) {
            self.active.insert(invocation);
            let gate = self
                .gates
                .entry(key.clone())
                .or_insert_with(|| ForkGate::new(max_parallel));
            gate.queued.push_back(invocation);
            gate.wait_for_slot(key, self.admit_tx.clone());
            return Ok(());
        }
        self.dispatch_invocation(invocation, None, running).await
    }

    /// A fork gate's waiter took a slot: the child at the head of the queue
    /// starts on it, and the gate waits again for the next one.
    async fn dispatch_admitted(
        &mut self,
        admitted: AdmittedSlot,
        running: &mut JoinSet<CompletedExecution>,
    ) -> Result<(), CoordinatorError> {
        let AdmittedSlot { gate: key, permit } = admitted;
        let Some(gate) = self.gates.get_mut(&key) else {
            return Ok(());
        };
        gate.waiter = None;
        let Some(invocation) = gate.queued.pop_front() else {
            return Ok(());
        };
        gate.wait_for_slot(key, self.admit_tx.clone());
        self.dispatch_invocation(invocation, Some(permit), running)
            .await
    }

    /// Declare the invocation's first execution when it has none, then run
    /// its driver. `slot` is the fork slot a gated child was admitted on.
    async fn dispatch_invocation(
        &mut self,
        invocation: InvocationId,
        slot: Option<OwnedSemaphorePermit>,
        running: &mut JoinSet<CompletedExecution>,
    ) -> Result<(), CoordinatorError> {
        let state = &self.store.state().invocations[&invocation];
        if state.executions.is_empty() {
            self.declare_first_execution(invocation).await?;
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
        let graph = self
            .store
            .load_graph(
                self.store.state().invocations[&invocation]
                    .declaration
                    .graph,
            )
            .await?;
        let PreparedExecution {
            driver,
            pipeline,
            cancel_before_run,
            slot,
        } = self
            .prepare_execution(
                invocation,
                execution,
                &declaration.start,
                declaration.middleware_state,
                &graph,
                slot,
            )
            .await?;
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
                slot,
            }
        });
        Ok(())
    }

    async fn complete_execution(
        &mut self,
        done: CompletedExecution,
        running: &mut JoinSet<CompletedExecution>,
        releasing: &mut JoinSet<InvocationReleased>,
    ) -> Result<(), CoordinatorError> {
        let CompletedExecution {
            invocation,
            execution,
            graph,
            report,
            middleware_state,
            slot,
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
            })
            .await?;
        }
        // The fork slot goes back only now, after the end is recorded, so the
        // next queued child's `ExecutionDeclared` follows this
        // `ExecutionFinished`.
        drop(slot);
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
                    )
                    .await?;
                }
                self.start_invocation(invocation, running).await?;
            }
            EngineExit::Terminal { status } => {
                let result = project_result(execution, status, &graph, &report.state);
                self.refuse_secret(&result)?;
                self.append(CoordinatorEvent::InvocationFinished {
                    invocation,
                    result: result.clone(),
                })
                .await?;
                self.release_invocation_leases(invocation, result.status, releasing)
                    .await;
                if invocation == InvocationId::ROOT {
                    self.last_root_report = Some(report);
                }
            }
        }
        Ok(())
    }

    /// Record what the execution's run-level hook points noted, in their
    /// order, before the run's own finish is recorded.
    async fn append_run_notes(
        &mut self,
        execution: ExecutionId,
        report: &driver::ExecutionReport,
    ) -> Result<(), CoordinatorError> {
        for note in &report.run_notes {
            self.append(CoordinatorEvent::RunNoteRecorded {
                execution: Some(execution),
                kind:      note.kind.clone(),
                payload:   note.payload.clone(),
            })
            .await?;
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

    async fn declare_first_execution(
        &mut self,
        invocation: InvocationId,
    ) -> Result<ExecutionId, CoordinatorError> {
        let execution = self.store.state().next_execution_id();
        let declaration = &self.store.state().invocations[&invocation].declaration;
        let start = EngineStart {
            entry:           EntryPoint::GraphEntries,
            context:         declaration.context.clone(),
            prior_firings:   BTreeMap::new(),
            execution_index: 0,
            max_executions:  self.options.max_executions,
        };
        self.append(CoordinatorEvent::ExecutionDeclared {
            execution,
            invocation,
            predecessor: None,
            start,
            middleware_state: initial_middleware_state(&self.middleware),
        })
        .await?;
        Ok(execution)
    }

    async fn declare_successor(
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
        })
        .await?;
        Ok(execution)
    }

    async fn prepare_execution(
        &mut self,
        invocation: InvocationId,
        execution: ExecutionId,
        start: &EngineStart,
        middleware_state: MiddlewareState,
        graph: &Graph,
        slot: Option<OwnedSemaphorePermit>,
    ) -> Result<PreparedExecution, CoordinatorError> {
        let mut cancel_before_run = self.store.state().invocations[&invocation].cancelled;
        let directory = self.execution_dir(execution);
        fs::create_dir_all(&directory).map_err(|source| CoordinatorError::Io {
            action: "create",
            path: directory.clone(),
            source,
        })?;
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
        let decoded = read_execution_log(&**self.store.logs(), execution).await?;
        let context = self.hook_context(invocation, execution);
        let (driver, writer): (driver::Driver, Arc<ExecutionLogWriter>) = if decoded.log.is_empty()
        {
            let writer = Arc::new(ExecutionLogWriter::new(self.writer.clone(), execution, 0));
            let sandbox = self
                .prepare_sandbox(invocation, execution, writer.clone())
                .await?;
            let driver = self.runtime.driver(
                (*graph).clone(),
                start.clone(),
                &directory,
                context,
                sandbox,
                secrets,
            );
            (driver, writer)
        } else {
            // Replaying an existing root cancellation already restores it.
            // Sending it again would ask the driver to escalate to a kill.
            cancel_before_run &= !decoded.log.events().any(|event| {
                matches!(
                    event,
                    Event::CancelRequested {
                        target: engine::CancelTarget::Scope(scope)
                    }
                    | Event::KillRequested { scope }
                        if *scope == ir::CancelScopeId::ROOT
                )
            });
            if !pipeline.is_empty() {
                rebuild_middleware(&pipeline, graph, &decoded.log).map_err(|error| {
                    CoordinatorError::EventWriter {
                        execution,
                        message: error.to_string(),
                    }
                })?;
            }
            let high_water = decoded.log.len() as u64;
            let writer = Arc::new(ExecutionLogWriter::new(
                self.writer.clone(),
                execution,
                high_water,
            ));
            let sandbox = self
                .prepare_sandbox(invocation, execution, writer.clone())
                .await?;
            let (driver, _) = self.runtime.resume_driver(
                (*graph).clone(),
                decoded.log,
                &directory,
                context,
                sandbox,
                secrets,
            )?;
            (driver.with_engine_start(start.clone()), writer)
        };
        let client = CoordinatorInvocationClient::new(execution, self.start_tx.clone());
        let identity = crate::ExecutionIdentity {
            run: self.runtime.run_key().clone(),
            invocation,
            execution,
        };
        let fold = Arc::new(pipeline.fold_observer());
        let mut driver = driver
            .with_run_owner(invocation == InvocationId::ROOT)
            .observe(writer.clone())
            .observe(fold)
            .with_decision_resolver(pipeline.clone())
            .with_capability(client)
            .with_capability(identity);
        let mut execution_slot = None;
        if let Some(slots) = self.attempt_slots(invocation) {
            let held = slot.map_or_else(ExecutionSlot::empty, ExecutionSlot::holding);
            driver = driver.with_attempt_slots(slots, held.clone());
            execution_slot = Some(held);
        }
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
            slot: execution_slot,
        })
    }

    /// What the hooks are told about an execution: the run, the invocation,
    /// the execution, and the call that started a nested invocation.
    fn hook_context(&self, invocation: InvocationId, execution: ExecutionId) -> HookContext {
        let context = HookContext::new(self.runtime.run_key().clone(), invocation, execution);
        match &self.store.state().invocations[&invocation].declaration.call {
            Some(call) => context.with_parent(ParentLink::from(call)),
            None => context,
        }
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
    async fn prepare_sandbox(
        &mut self,
        invocation: InvocationId,
        execution: ExecutionId,
        writer: Arc<ExecutionLogWriter>,
    ) -> Result<SandboxAssignment, CoordinatorError> {
        match self.store.state().invocations[&invocation]
            .declaration
            .sandbox
        {
            SandboxBinding::Inherited { lease } => {
                let (workspace, runtime) = {
                    let resources = self.resources().await;
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
                    self.cancel_invocations(vec![(invocation, false)], invocation, None, false)
                        .await?;
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
                || declaration.admission != request.request.admission
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
                self.handle_cancel(CancelRequest {
                    invocation: previous,
                    reason:     None,
                    escalate:   false,
                })
                .await
                .map_err(invoke_error)?;
                return Ok(StartOutcome::Requeue { previous });
            }
        }
        let total = self.store.state().invocations.len() as u64;
        if total >= u64::from(self.options.max_invocations) {
            return Err(InvokeError::InvocationLimit {
                total,
                limit: self.options.max_invocations,
                parent: key.parent,
                firing: key.firing,
                slot: key.slot.clone(),
            });
        }
        let invocation = self.store.state().next_invocation_id();
        let sandbox = match request.request.sandbox {
            SandboxMode::Isolated => SandboxBinding::Isolated,
            SandboxMode::Inherit { scope } => match self.inherited_binding(key, scope).await {
                Ok(binding) => binding,
                Err(CoordinatorError::NoInheritableSandbox) => {
                    return Err(InvokeError::NoInheritableSandbox);
                }
                Err(error) => return Err(invoke_error(error)),
            },
        };
        if let SandboxBinding::Inherited { lease } = sandbox {
            self.check_inherited_container(lease, request.request.graph)
                .await?;
        }
        self.append(CoordinatorEvent::InvocationDeclared {
            invocation,
            call: Some(key.clone()),
            graph: request.request.graph,
            context: request.request.context.clone(),
            secret_bindings: request.request.secrets.clone(),
            sandbox,
            admission: request.request.admission.clone(),
        })
        .await
        .map_err(invoke_error)?;
        Ok(StartOutcome::Attach {
            invocation,
            is_new: true,
        })
    }

    /// Record a pause or unpause when it changes the recorded state, then
    /// tell the requester. The reply is sent after the append and after
    /// every observer saw the record, so an observer-derived event is queued
    /// before the requester acts on it.
    async fn handle_pause(&mut self, request: PauseRequest) -> Result<(), CoordinatorError> {
        let PauseRequest { paused, reply } = request;
        if self.store.state().paused != paused {
            self.append(if paused {
                CoordinatorEvent::RunPaused
            } else {
                CoordinatorEvent::RunUnpaused
            })
            .await?;
        }
        let _ = reply.send(());
        Ok(())
    }

    async fn handle_cancel(&mut self, request: CancelRequest) -> Result<(), CoordinatorError> {
        let CancelRequest {
            invocation: cancelled,
            reason,
            escalate,
        } = request;
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
        self.cancel_invocations(affected, cancelled, reason, escalate)
            .await
    }

    /// Record the cancel of every affected invocation; the reason goes on
    /// the one the requester named, the descendants follow from it. The
    /// drivers of the newly cancelled invocations are told to cancel; a
    /// driver that was already cancelled is told again only when the request
    /// escalates, which is what reaches its kill tier.
    async fn cancel_invocations(
        &mut self,
        affected: Vec<(InvocationId, bool)>,
        requested: InvocationId,
        reason: Option<CancelReason>,
        escalate: bool,
    ) -> Result<(), CoordinatorError> {
        for (invocation, already_cancelled) in &affected {
            if !already_cancelled {
                self.append(CoordinatorEvent::InvocationCancelRequested {
                    invocation: *invocation,
                    reason:     (*invocation == requested).then(|| reason.clone()).flatten(),
                })
                .await?;
            }
        }

        let affected: BTreeSet<_> = affected
            .into_iter()
            .filter(|(_, already_cancelled)| escalate || !already_cancelled)
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
    async fn inherited_binding(
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
            self.resources().await.resolve_usable(lease)?;
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
        self.resources().await.resolve_usable(lease)?;
        Ok(SandboxBinding::Inherited { lease })
    }

    /// An inherited child runs in its caller's sandbox, so a container it
    /// declares must be the caller's own: absent (the scope takes the
    /// caller's target) or equal. A different image or option set is a
    /// deterministic refusal at declaration; a child that needs its own
    /// image uses an isolated binding.
    async fn check_inherited_container(
        &mut self,
        lease: crate::SandboxLeaseId,
        child: GraphDigest,
    ) -> Result<(), InvokeError> {
        let parent = self
            .resources()
            .await
            .resolve(lease)
            .map(|record| record.runtime.clone())
            .map_err(invoke_error)?;
        let graph = self.store.load_graph(child).await.map_err(invoke_error)?;
        for scope in &graph.scopes {
            let declares_container =
                matches!(scope.runtime.target, RuntimeTarget::Container { .. });
            if declares_container && scope.runtime.target != parent.target {
                return Err(InvokeError::InheritedContainerMismatch { scope: scope.id });
            }
        }
        Ok(())
    }

    /// The fork gate an invocation was declared under, and its slot count,
    /// when its declaration bounds concurrency.
    fn fork_admission(&self, invocation: InvocationId) -> Option<(GateKey, u32)> {
        let declaration = &self.store.state().invocations[&invocation].declaration;
        let admission = declaration.admission.as_ref()?;
        let parent = declaration.call.as_ref()?.parent;
        Some(((parent, admission.gate.clone()), admission.max_parallel))
    }

    /// The slots an invocation's driver shares with its fork's other
    /// children: one semaphore per parent execution and gate name, created
    /// with the first child that names it.
    fn attempt_slots(&mut self, invocation: InvocationId) -> Option<Arc<Semaphore>> {
        let (key, max_parallel) = self.fork_admission(invocation)?;
        Some(
            self.gates
                .entry(key)
                .or_insert_with(|| ForkGate::new(max_parallel))
                .slots
                .clone(),
        )
    }

    /// Drop every fork gate whose parent execution is no longer live and
    /// that has no child queued or live. A gate still in use keeps its
    /// slots, so the bound holds for children that settle after their
    /// parent, and a child that restarts rejoins the same gate.
    fn drop_idle_gates(&mut self) {
        let in_use: BTreeSet<GateKey> = self
            .active
            .iter()
            .filter_map(|invocation| self.fork_admission(*invocation).map(|(key, _)| key))
            .collect();
        let live = &self.active_handles;
        self.gates.retain(|key, gate| {
            live.contains_key(&key.0) || !gate.queued.is_empty() || in_use.contains(key)
        });
    }

    async fn append(
        &mut self,
        event: CoordinatorEvent,
    ) -> Result<CoordinatorRecord, CoordinatorError> {
        let record = self.store.append(event).await?;
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

async fn validate_resources(
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
        let graph = store.load_graph(digest).await?;
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
                    let log = read_execution_log(&**store.logs(), execution).await?;
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
    let mut updates = BTreeMap::new();
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
                |record| {
                    // The result node's own writes, from its final record:
                    // the node record keeps the output, the history the
                    // outcome.
                    if let Some(final_record) = state
                        .history()
                        .iter()
                        .rev()
                        .find(|final_record| final_record.node == node)
                    {
                        updates = final_record.outcome.context_updates.clone();
                    }
                    record.output.clone()
                },
            ),
    };
    InvocationResult {
        status,
        failure,
        final_execution: execution,
        output,
        context: (*state.run_context().kv).clone(),
        updates,
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
