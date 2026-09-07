//! The driver loop.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt::Debug;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engine::{
    Admission, CANCEL_ESCALATION_KEY, Command, DecisionId, EngineExit, EngineStart, EngineState,
    Event, EventLog, GroupDecision, ReplayMismatch, ResolvedFiring, RouteDecision, apply,
};
use executor::{
    AcquireContext, EnvError, EnvHandle, EnvironmentId, Executor, NoProgress, ProgressSink,
    ReleaseReport, Retention, SandboxLeaseId, ScopeOutcome, ScopeSpec, SecretProvider, WorkspaceId,
};
use ir::placeholder::SECRET_REF_KEY;
use ir::{
    Attempt, Control, EvalEnv, ExprOrValue, FailureClass, FailureInfo, FiringId, Graph, NodeId,
    Outcome, RunContext, RunStatus, RuntimeSpec, ScopeId, StaticCtx, Status, StepEvent, Value,
    eval,
};
use smol_str::SmolStr;
use steps::{Capabilities, Registry, StepCtx};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio::time;
use tracing::Instrument as _;

use crate::jitter::jittered;
use crate::lifecycle::{
    AdmitAttempt, AttemptDecision, ExecutionHooks, Note, PrepareError, PrepareResult, Prepared,
    RESULT_PREPARATION_CLASS, RESULT_PREPARED_KIND, Recorded, ResultOrigin, ResultPreparedNote,
    TRANSITION_KIND, Transition, TransitionNote, apply_transition,
};
use crate::observe::{EventObserver, ObserveError};
use crate::sink::LogSink;
use crate::view::{BranchMap, live_view, routing_view};
use crate::{
    AdmissionResolution, AdmitRequest, DecisionResolver, DefaultDecisionResolver, RoutingRequest,
    RoutingResolution,
};

/// The failure class recorded when the driver had to abort a step that ignored
/// `Control::Cancel`.
pub const CANCEL_FORCED: FailureClass = FailureClass::new_static("cancel_forced");

/// The `cancel_escalation` value on a firing the polite tier had marked
/// cancelling when the driver died: resume never re-spawns it — re-spawning
/// real work only to stop it records nothing the stop had not already decided —
/// and finishes it directly with the same `Cancelled` outcome a live cancel
/// would have fed. Routes exactly as a live cancel's outcome would.
pub const CANCELLED_BEFORE_RESUME: FailureClass =
    FailureClass::new_static("cancelled_before_resume");

/// The kill-tier counterpart of [`CANCELLED_BEFORE_RESUME`]: same direct
/// finish, and — the tier coming from the replayed state, where killed scopes
/// are in the log — the outcome is recorded without routing, as a live kill's
/// would be.
pub const KILLED_BEFORE_RESUME: FailureClass = FailureClass::new_static("killed_before_resume");

/// No step kind is registered for a node's `StepRef.kind`.
///
/// `validate_with(graph, Some(&registry))` reports this at load; the guard here
/// is the backstop for a caller that skipped validation.
pub(crate) const NO_RUNNER: FailureClass = FailureClass::new_static("no_runner");

/// After the first root cancel, how long admitted cleanup gets before the
/// driver feeds back `KillRequested` (§10, resolved decision 3).
pub const DEFAULT_CLEANUP_GRACE: Duration = Duration::from_secs(120);

/// Capacity of a firing's control channel. A named implementation constant, not
/// a compatibility rule: reliable delivery and ordering hold when the channel
/// is full, because every send rides the firing's serialized forwarder.
pub const CONTROL_CHANNEL_CAPACITY: usize = 32;

/// Capacity of the driver's signal channel — every executor and step signal
/// (step events, finishes, retry and deadline timers, host cancels) funnels
/// through it into the driver loop. Generous, so a burst of concurrent
/// firings' progress buffers while the loop applies a record; the senders are
/// all tasks that tolerate awaiting capacity, so fullness costs latency, never
/// ordering or delivery.
const SIGNAL_CHANNEL_CAPACITY: usize = 1024;

/// Knobs, with the defaults from the handoff's table.
#[derive(Clone, Debug)]
pub struct RunConfig {
    pub run_dir:             PathBuf,
    /// Between `SIGTERM` and `SIGKILL`, per scope.
    pub grace:               Duration,
    /// How much longer than `grace` a step gets before the driver stops
    /// waiting.
    pub hard_deadline_slack: Duration,
    /// Between the first root cancel and the `KillRequested` that ends whatever
    /// cleanup is still running.
    pub cleanup_grace:       Duration,
    pub keep_workspaces:     Retention,
    /// Echo step output to this process's stdout.
    pub echo_logs:           bool,
    /// Prefix for process and container fences. An execution supplies its ID.
    pub environment_prefix:  Option<SmolStr>,
    /// Prefix for persistent workspaces. An invocation supplies its ID.
    pub workspace_prefix:    Option<SmolStr>,
    /// Exact inherited workspace for every scope in this execution.
    pub workspace_override:  Option<WorkspaceId>,
    /// The runtime target every scope in this execution runs on, when an
    /// inherited sandbox decides it instead of the graph.
    pub runtime_override:    Option<RuntimeSpec>,
    /// The durable sandbox lease each scope acquires under. A coordinator
    /// supplies them; a bare driver has none, and its executor then keys
    /// each sandbox by the scope alone and ends it with the scope.
    pub scope_leases:        ScopeLeases,
}

/// Which durable lease each scope of an execution acquires its sandbox under.
#[derive(Clone, Debug, Default)]
pub enum ScopeLeases {
    /// No coordinator: every sandbox is the scope's own.
    #[default]
    None,
    /// Reserve invocation-owned leases as original or dynamic scopes acquire.
    Owned(Arc<dyn ScopeLeaseAllocator>),
    /// One inherited lease for every scope: the caller's sandbox.
    Shared(SandboxLeaseId),
}

/// Durable lease and workspace chosen before an executor acquires a scope.
pub struct ScopeLease {
    pub lease:     SandboxLeaseId,
    pub workspace: WorkspaceId,
}

/// The coordinator's resource authority. Allocation completes durably before
/// a scope can expose its sandbox to a step.
#[async_trait::async_trait]
pub trait ScopeLeaseAllocator: Debug + Send + Sync {
    async fn reserve(
        &self,
        identity: engine::ScopeIdentity,
        spec: &ScopeSpec,
    ) -> Result<ScopeLease, EnvError>;
}

/// Everything a coordinator decides about where an execution's scopes run:
/// the inherited workspace and runtime target, and the leases. `Default` is
/// what a bare driver gets: the graph's own targets, no leases.
#[derive(Clone, Debug, Default)]
pub struct SandboxAssignment {
    pub workspace_override: Option<WorkspaceId>,
    pub runtime_override:   Option<RuntimeSpec>,
    pub leases:             ScopeLeases,
}

impl RunConfig {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir:             run_dir.into(),
            grace:               executor::DEFAULT_GRACE,
            hard_deadline_slack: Duration::from_secs(5),
            cleanup_grace:       DEFAULT_CLEANUP_GRACE,
            keep_workspaces:     Retention::default(),
            echo_logs:           false,
            environment_prefix:  None,
            workspace_prefix:    None,
            workspace_override:  None,
            runtime_override:    None,
            scope_leases:        ScopeLeases::None,
        }
    }

    #[must_use]
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    #[must_use]
    pub fn with_cleanup_grace(mut self, cleanup_grace: Duration) -> Self {
        self.cleanup_grace = cleanup_grace;
        self
    }

    #[must_use]
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.keep_workspaces = retention;
        self
    }

    #[must_use]
    pub fn with_echo(mut self, echo: bool) -> Self {
        self.echo_logs = echo;
        self
    }

    #[must_use]
    pub fn with_scope_identities(
        mut self,
        environment_prefix: impl Into<SmolStr>,
        workspace_prefix: impl Into<SmolStr>,
    ) -> Self {
        self.environment_prefix = Some(environment_prefix.into());
        self.workspace_prefix = Some(workspace_prefix.into());
        self
    }

    #[must_use]
    pub fn with_workspace_override(mut self, workspace: WorkspaceId) -> Self {
        self.workspace_override = Some(workspace);
        self
    }

    /// Apply a coordinator's sandbox decisions in one step.
    #[must_use]
    pub fn with_sandbox_assignment(mut self, assignment: SandboxAssignment) -> Self {
        self.workspace_override = assignment.workspace_override;
        self.runtime_override = assignment.runtime_override;
        self.scope_leases = assignment.leases;
        self
    }
}

/// Why a log could not be resumed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResumeError {
    #[error(transparent)]
    Replay(#[from] ReplayMismatch),
}

/// What a host learns from [`Driver::resume`], before calling `run()`.
pub struct ResumeInfo {
    /// The firings whose step will actually execute again. Resume is invisible
    /// in the log, so this is where the host mints its own new execution
    /// identity for each — without any log event.
    pub redispatched: Vec<FiringId>,
    /// The rebuilt log: the loaded prefix plus the regenerated suffix. A host
    /// whose own durable ingest lagged further behind than the loaded log
    /// catches up from here before attaching.
    pub log:          EventLog,
    /// How many records were loaded; everything past them was regenerated.
    pub loaded:       usize,
}

/// What a resumed driver owes before entering the normal loop.
struct PendingResume {
    commands:    Vec<Command>,
    /// Where the loaded log ended. Records past this are the regenerated
    /// suffix, handed to observers before any pending command is dispatched.
    suffix_from: usize,
}

/// How a run ended, and what it left behind.
pub struct ExecutionReport {
    pub exit:            EngineExit,
    pub status:          RunStatus,
    pub state:           EngineState,
    pub releases:        Vec<ReleaseReport>,
    /// What each failing observer's `finish` reported. Never changes `status`:
    /// a host with fatal-sink semantics watches its own observer and cancels.
    pub observer_errors: Vec<ObserveError>,
}

/// Why a step was told to stop. The distinction cannot be made by the step —
/// only the driver knows which arrived first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelReason {
    Requested,
    TimedOut,
}

/// How a host-delivered control landed. Hosts retry or report per class:
/// answers are must-deliver, steering is best-effort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliverDisposition {
    /// The value reached the firing's control channel.
    Delivered,
    /// The firing was not live — unknown, finished, cancelling, awaiting a
    /// retry — or it ended before the delivery cleared the channel.
    NotLive,
}

type DeliverAck = oneshot::Sender<DeliverDisposition>;

/// Cancel a running run, or deliver a value into one of its firings, from
/// outside.
#[derive(Clone)]
pub struct RunHandle {
    tx: mpsc::Sender<Signal>,
}

impl RunHandle {
    /// Request polite cancellation of the group containing a named node.
    /// Returns false when the node has no group or the driver has stopped.
    pub async fn cancel_group(&self, node: impl Into<SmolStr>) -> bool {
        let (ack, accepted) = oneshot::channel();
        if self
            .tx
            .send(Signal::CancelGroup {
                node: node.into(),
                ack,
            })
            .await
            .is_err()
        {
            return false;
        }
        accepted.await.unwrap_or(false)
    }

    /// Cancel a scope. `CancelScopeId::ROOT` cancels the whole run.
    pub async fn cancel(&self, scope: ir::CancelScopeId) {
        let _ = self
            .tx
            .send(Signal::Inject(Event::CancelRequested { scope }))
            .await;
    }

    /// Deliver a control to a live firing — a human gate's answer, a
    /// supervisor's steering — through the engine, so question and answer
    /// are both in the log.
    ///
    /// The disposition is completed by the firing's forwarder: `Delivered` only
    /// after the value is in the firing's control channel, `NotLive` when the
    /// core emitted no command (the event is still logged — the audit
    /// trail) or the firing ended before the send completed.
    pub async fn deliver(&self, firing: FiringId, ctl: Control) -> DeliverDisposition {
        let (ack, disposition) = oneshot::channel();
        if self
            .tx
            .send(Signal::Deliver { firing, ctl, ack })
            .await
            .is_err()
        {
            return DeliverDisposition::NotLive;
        }
        disposition.await.unwrap_or(DeliverDisposition::NotLive)
    }
}

/// Everything that reaches the loop, through one channel, in arrival order.
enum Signal {
    CancelGroup {
        node: SmolStr,
        ack:  oneshot::Sender<bool>,
    },
    /// An event from outside the run entirely, such as an operator cancelling.
    Inject(Event),
    /// A host delivers a value into a firing, and wants to know how it landed.
    Deliver {
        firing: FiringId,
        ctl:    Control,
        ack:    DeliverAck,
    },
    Progress {
        firing: FiringId,
        event:  StepEvent,
    },
    Finished {
        firing:  FiringId,
        attempt: Attempt,
        outcome: Outcome,
    },
    /// An attempt timer expired. `timer` names the arming; a timer that was
    /// paused or re-armed since is stale and ignored.
    Timeout {
        firing:  FiringId,
        attempt: Attempt,
        timer:   u64,
    },
    RetryDue {
        firing:       FiringId,
        next_attempt: Attempt,
    },
    Admitted {
        decision_id: DecisionId,
        resolution:  AdmissionResolution,
    },
    RoutingResolved {
        decision_id: DecisionId,
        resolution:  RoutingResolution,
    },
    HardDeadline {
        firing:  FiringId,
        attempt: Attempt,
    },
    AcquireFinished {
        scope:  ScopeId,
        id:     u64,
        result: Result<AcquiredScope, String>,
    },
    /// The host's `before_attempt` answered; the resolver runs next when it
    /// admitted.
    HookAdmitted {
        decision_id: DecisionId,
        decision:    AttemptDecision,
    },
    /// The host's `prepare_result` answered for a finished attempt.
    ResultPrepared {
        firing:  FiringId,
        attempt: Attempt,
        result:  Result<Prepared, PrepareError>,
    },
    /// A routing decision that went through `after_record`, the resolver and
    /// `transition`, with the notes to append before its record.
    HookedRoutingResolved {
        decision_id: DecisionId,
        resolution:  RoutingResolution,
        notes:       Vec<Note>,
    },
}

/// An acquired environment that has not reached the driver yet.
///
/// If the acquire task or its channel send is abandoned after the executor
/// returned, dropping this value still releases the environment. This closes
/// the narrow gap between `Executor::acquire` completing and the driver taking
/// ownership of its result.
struct AcquiredScope {
    abandoned: mpsc::UnboundedSender<EnvHandle>,
    handle:    Option<EnvHandle>,
}

impl AcquiredScope {
    fn new(handle: EnvHandle, abandoned: mpsc::UnboundedSender<EnvHandle>) -> Self {
        Self {
            abandoned,
            handle: Some(handle),
        }
    }

    fn into_handle(mut self) -> EnvHandle {
        self.handle
            .take()
            .expect("an acquired scope transfers its environment once")
    }
}

impl Drop for AcquiredScope {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let _ = self.abandoned.send(handle);
    }
}

/// One control send, queued on a firing's forwarder.
struct Forward {
    ctl: Control,
    /// Completed by the forwarder: `Delivered` once the send lands, `NotLive`
    /// when the receiver is gone. Stop signals carry no ack.
    ack: Option<DeliverAck>,
}

struct Task {
    name:     SmolStr,
    scope:    ScopeId,
    attempt:  Attempt,
    /// When the attempt was dispatched, for the observed duration a step
    /// kind did not measure itself. Driver-side only: the clock never enters
    /// the core, only the recorded number does, like any step metric.
    started:  time::Instant,
    /// The firing's `driver.step` span, held so the driver-loop events about
    /// this firing — the stop signal, the hard deadline, the finish — land
    /// inside it: they run on the driver task, not in the runner future the
    /// span is attached to.
    span:     tracing::Span,
    /// The firing's serialized forwarder: control sends await channel capacity
    /// here, in order, so the driver loop never blocks on a full channel. When
    /// the firing ends the receiver drops, pending sends fail, and their
    /// acks resolve `NotLive`.
    forwards: mpsc::UnboundedSender<Forward>,
    /// Owns the runner, its control and progress forwarders, and its timers.
    /// Dropping the firing aborts them all; a normal finish also joins them.
    workers:  JoinSet<()>,
    deadline: Option<AbortHandle>,
    reason:   Option<CancelReason>,
    /// The attempt's active-work budget, when the driver enforces it.
    budget:   Option<AttemptBudget>,
}

/// The per-attempt timer for an `ExecutorEnforced` node (§10). It counts
/// active work only: while the step has a question pending with the host the
/// clock stops, and it resumes with the remaining time once every pending
/// question is answered. Each arming has its own id, so an expiry from a
/// timer that was paused or re-armed in the meantime is ignored as stale.
struct AttemptBudget {
    remaining: Duration,
    /// When the running timer was armed; `None` while paused.
    armed_at:  Option<time::Instant>,
    timer:     Option<AbortHandle>,
    timer_id:  u64,
    /// Question ids awaiting an answer from the host.
    pending:   BTreeSet<String>,
}

/// A per-run host service riding a run's lifetime — an object-store listener,
/// a proxy, anything a provisioner starts beside the run dir. The driver holds
/// each guard untouched while the run executes and awaits
/// [`RunGuard::teardown`] before the report, so a service whose teardown joins
/// a thread never blocks a Tokio worker from `Drop`. A driver that never
/// finishes — a dropped `run` future — tears guards down after its aborted
/// tasks and environments when the runtime remains available. A guard's
/// `Drop` stays its safety net when the runtime is already gone.
#[async_trait::async_trait]
pub trait RunGuard: Send + Sync {
    /// Explicit async teardown. The default just drops `self`, so a guard
    /// whose `Drop` already tears down without blocking implements nothing.
    async fn teardown(self: Box<Self>) {}
}

struct ScopeAcquire {
    id:   u64,
    join: AbortHandle,
}

pub struct Driver {
    engine:           EngineState,
    executor:         Arc<dyn Executor>,
    runners:          Arc<Registry>,
    secrets:          Arc<dyn SecretProvider>,
    progress:         Arc<dyn ProgressSink>,
    decisions:        Arc<dyn DecisionResolver>,
    sink:             Arc<LogSink>,
    config:           RunConfig,
    envs:             HashMap<ScopeId, EnvHandle>,
    acquires:         HashMap<ScopeId, ScopeAcquire>,
    /// Acquisition, decision, retry, and signal tasks share the driver's
    /// lifetime. Completed tasks are reaped by the run loop.
    background:       JoinSet<()>,
    next_acquire_id:  u64,
    acquire_failures: HashMap<ScopeId, String>,
    pending_starts:   HashMap<ScopeId, Vec<ResolvedFiring>>,
    /// Deliveries that arrived before execution admission resolved. Unlike
    /// [`Forward`], every buffered delivery carries an ack.
    early_deliveries: HashMap<FiringId, Vec<(Control, DeliverAck)>>,
    pending_forwards: HashMap<FiringId, Vec<Forward>>,
    pending_failures: HashMap<FiringId, (Attempt, String)>,
    scope_failed:     HashSet<ScopeId>,
    tasks:            HashMap<FiringId, Task>,
    caps:             Capabilities,
    observers:        Vec<Arc<dyn EventObserver>>,
    /// Per-run host services riding this run's lifetime: held untouched until
    /// the run ends, when their teardown is awaited.
    run_guards:       Vec<Box<dyn RunGuard>>,
    guard_teardown:   Option<JoinHandle<()>>,
    releases:         Vec<JoinHandle<ReleaseReport>>,
    /// Armed by the first root cancel; expiry feeds back `KillRequested`.
    cleanup_timer:    Option<AbortHandle>,
    /// One id per attempt-timer arming, so a stale expiry is recognizable.
    next_timer_id:    u64,
    decision_tasks:   HashMap<DecisionId, AbortHandle>,
    /// The host's awaited extension points, when installed.
    hooks:            Option<Arc<dyn ExecutionHooks>>,
    /// Branch roles over the live graph, for the views hooks see. Recomputed
    /// when a splice grows the graph.
    branches:         BranchMap,
    /// Finished attempts whose result the host is still preparing: the
    /// outcome as reported, recorded as-is if the run is killed first.
    preparing:        HashMap<FiringId, (Attempt, Outcome, AbortHandle)>,
    start:            EngineStart,
    /// Set by [`Driver::resume`]; consumed at the top of [`Driver::run`].
    resume:           Option<PendingResume>,
    tx:               mpsc::Sender<Signal>,
    rx:               mpsc::Receiver<Signal>,
    /// An acquire aborted after completing transfers its environment here.
    /// The queue holds at most one result per unfinished acquire task and is
    /// drained by the run loop and after all acquires have been joined.
    abandoned_tx:     mpsc::UnboundedSender<EnvHandle>,
    abandoned_rx:     mpsc::UnboundedReceiver<EnvHandle>,
}

impl Drop for Driver {
    fn drop(&mut self) {
        // Abort before scheduling cleanup: even if its first poll is delayed,
        // runners must no longer start work or retain the run's services.
        self.background.abort_all();
        for task in self.tasks.values_mut() {
            task.workers.abort_all();
        }
        self.rx.close();
        let (_, empty) = mpsc::channel(1);
        let mut signals = mem::replace(&mut self.rx, empty);

        let mut background = mem::take(&mut self.background);
        let tasks = mem::take(&mut self.tasks);
        let mut envs: Vec<_> = mem::take(&mut self.envs).into_values().collect();
        let releases = mem::take(&mut self.releases);
        let guards = mem::take(&mut self.run_guards);
        let guard_teardown = self.guard_teardown.take();
        let (_, empty) = mpsc::unbounded_channel();
        let mut abandoned = mem::replace(&mut self.abandoned_rx, empty);
        if background.is_empty()
            && tasks.is_empty()
            && envs.is_empty()
            && releases.is_empty()
            && guards.is_empty()
            && guard_teardown.is_none()
            && abandoned.is_empty()
            && signals.is_empty()
        {
            return;
        }
        let executor = self.executor.clone();
        if let Ok(runtime) = Handle::try_current() {
            // Drop cannot await. This is the safety net for an abandoned run;
            // normal completion joins tasks and releases resources in `run`.
            runtime.spawn(async move {
                background.shutdown().await;
                for (_, mut task) in tasks {
                    task.workers.shutdown().await;
                }
                while let Ok(signal) = signals.try_recv() {
                    if let Signal::AcquireFinished {
                        result: Ok(acquired),
                        ..
                    } = signal
                    {
                        envs.push(acquired.into_handle());
                    }
                }
                drop(signals);
                while let Ok(handle) = abandoned.try_recv() {
                    envs.push(handle);
                }
                for env in envs {
                    let _ = executor.release(env, ScopeOutcome::Failed).await;
                }
                for release in releases {
                    let _ = release.await;
                }
                for guard in guards {
                    guard.teardown().await;
                }
                if let Some(teardown) = guard_teardown {
                    let _ = teardown.await;
                }
            });
        }
    }
}

/// A stop's outcome: `status`, with the escalation that ended the step in the
/// output — `Status::Cancelled` carries no `FailureInfo`, so the tag rides the
/// output (§3.1 rule 5).
fn escalation_outcome(status: Status, escalation: &FailureClass) -> Outcome {
    Outcome::new(
        status,
        serde_json::json!({ CANCEL_ESCALATION_KEY: escalation }),
    )
}

impl Driver {
    pub fn new(
        graph: Graph,
        executor: Arc<dyn Executor>,
        runners: Registry,
        secrets: Arc<dyn SecretProvider>,
        config: RunConfig,
    ) -> Self {
        Self::with_state(EngineState::new(graph), executor, runners, secrets, config)
    }

    /// Continue a run whose process died — the primary resume API (§10). The
    /// host hands in the graph and log however it stored them; replay rebuilds
    /// the state and regenerates the commands still owed (held scopes are
    /// re-acquired, live firings re-dispatched, pending retries re-armed —
    /// timers restart in full, the log has no clock). `ResumeInfo` comes back
    /// beside the driver so the host installs its own execution identities for
    /// the re-dispatched firings *before* calling `run()`.
    ///
    /// An empty log resumes as a fresh start: nothing durable happened.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the host hands the dead run's graph and log over together; the log it keeps \
                  afterwards is the rebuilt one in `ResumeInfo`, not the loaded prefix"
    )]
    #[tracing::instrument(
        name = "driver.resume",
        skip_all,
        fields(run_dir = %config.run_dir.display(), record_count = log.len())
    )]
    pub fn resume(
        graph: Graph,
        log: EventLog,
        executor: Arc<dyn Executor>,
        runners: Registry,
        secrets: Arc<dyn SecretProvider>,
        config: RunConfig,
    ) -> Result<(Self, ResumeInfo), ResumeError> {
        let point = engine::resume(graph, &log)?;
        tracing::info!(
            record_count = log.len(),
            pending_count = point.pending.len(),
            "run resumed"
        );
        let info = ResumeInfo {
            redispatched: point.redispatched,
            log:          point.state.log.clone(),
            loaded:       log.len(),
        };
        let mut driver = Self::with_state(point.state, executor, runners, secrets, config);
        if !log.is_empty() {
            driver.resume = Some(PendingResume {
                commands:    point.pending,
                suffix_from: log.len(),
            });
        }
        Ok((driver, info))
    }

    fn with_state(
        engine: EngineState,
        executor: Arc<dyn Executor>,
        runners: Registry,
        secrets: Arc<dyn SecretProvider>,
        config: RunConfig,
    ) -> Self {
        let start = engine.start().cloned().unwrap_or_default();
        let sink =
            Arc::new(LogSink::new(&config.run_dir, secrets.masker()).with_echo(config.echo_logs));
        let (tx, rx) = mpsc::channel(SIGNAL_CHANNEL_CAPACITY);
        let (abandoned_tx, abandoned_rx) = mpsc::unbounded_channel();
        Self {
            engine,
            executor,
            runners: Arc::new(runners),
            secrets,
            progress: Arc::new(NoProgress),
            decisions: Arc::new(DefaultDecisionResolver),
            sink,
            config,
            envs: HashMap::new(),
            acquires: HashMap::new(),
            background: JoinSet::new(),
            next_acquire_id: 0,
            acquire_failures: HashMap::new(),
            pending_starts: HashMap::new(),
            early_deliveries: HashMap::new(),
            pending_forwards: HashMap::new(),
            pending_failures: HashMap::new(),
            scope_failed: HashSet::new(),
            tasks: HashMap::new(),
            caps: Capabilities::default(),
            observers: Vec::new(),
            run_guards: Vec::new(),
            guard_teardown: None,
            releases: Vec::new(),
            cleanup_timer: None,
            next_timer_id: 0,
            decision_tasks: HashMap::new(),
            hooks: None,
            branches: BranchMap::default(),
            preparing: HashMap::new(),
            start,
            resume: None,
            tx,
            rx,
            abandoned_tx,
            abandoned_rx,
        }
    }

    /// Register an observer: it sees every appended record, in seq order, with
    /// the post-apply state, and its `finish` is awaited before the report.
    #[must_use]
    pub fn observe(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.observers.push(observer);
        self
    }

    /// The host services every step's `StepCtx` carries (default: none).
    #[must_use]
    pub fn with_capabilities(mut self, caps: Capabilities) -> Self {
        self.caps = caps;
        self
    }

    /// Add one capability that is specific to this execution.
    #[must_use]
    pub fn with_capability<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.caps = self.caps.with(value);
        self
    }

    /// Hold a per-run host service for this run's lifetime. The driver never
    /// looks inside; when the run ends it awaits the guard's
    /// [`RunGuard::teardown`]. Dropping the driver mid-run schedules the same
    /// teardown after its steps and environments have stopped.
    #[must_use]
    pub fn with_run_guard(mut self, guard: Box<dyn RunGuard>) -> Self {
        self.run_guards.push(guard);
        self
    }

    /// Where live acquisition progress goes — image pulls, service health.
    /// Wall-clock effects only; nothing of it enters the replay log.
    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn ProgressSink>) -> Self {
        self.progress = progress;
        self
    }

    /// Set the complete start specification for a fresh execution.
    #[must_use]
    pub fn with_engine_start(mut self, start: EngineStart) -> Self {
        self.start = start;
        self
    }

    /// Install the host admission and routing pipeline.
    #[must_use]
    pub fn with_decision_resolver(mut self, resolver: Arc<dyn DecisionResolver>) -> Self {
        self.decisions = resolver;
        self
    }

    /// Install the host's awaited extension points (`lifecycle`). Without
    /// them the driver takes the unchanged fast path: no callback, no extra
    /// task, no extra record.
    #[must_use]
    pub fn with_hooks(mut self, hooks: Arc<dyn ExecutionHooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Run to completion.
    ///
    /// The loop's state is boxed here so the future a caller holds stays
    /// small whatever the driver carries.
    pub async fn run(self) -> ExecutionReport {
        Box::pin(self.run_loop()).await
    }

    #[tracing::instrument(
        name = "driver.run",
        skip_all,
        fields(
            run_dir = %self.config.run_dir.display(),
            resumed = self.resume.is_some(),
            node_count = self.engine.graph().nodes.len(),
        )
    )]
    async fn run_loop(mut self) -> ExecutionReport {
        self.caps = self.caps.with(self.handle());
        match self.resume.take() {
            None => self.feed(Event::ExecutionStarted(self.start.clone())),
            Some(resume) => {
                // Observers see the regenerated suffix first — the records past
                // the loaded prefix, which the crash kept off disk — before any
                // pending command is dispatched, so a store attaching here
                // starts from a converged view.
                self.notify_observers(resume.suffix_from);
                for command in resume.commands {
                    self.dispatch(command);
                }
                // Like retry and step timers, cleanup gets its full grace
                // after a crash. Rearm it without feeding a second cancel,
                // which would escalate the restored run to a hard kill.
                if self.engine.is_cancelled() && !self.engine.is_finished() {
                    self.arm_cleanup_timer();
                }
            }
        }

        while !self.engine.is_finished() {
            let signal = tokio::select! {
                signal = self.rx.recv() => signal,
                Some(handle) = self.abandoned_rx.recv() => {
                    self.spawn_release(handle, ScopeOutcome::Failed);
                    continue;
                }
                joined = self.background.join_next(), if !self.background.is_empty() => {
                    if let Some(Err(error)) = joined
                        && !error.is_cancelled()
                    {
                        tracing::warn!(error = ?error, "driver background task failed");
                    }
                    continue;
                }
            };
            let Some(signal) = signal else {
                break;
            };
            self.on_signal(signal).await;
        }
        for (_, deliveries) in self.early_deliveries.drain() {
            for (_, ack) in deliveries {
                let _ = ack.send(DeliverDisposition::NotLive);
            }
        }
        self.abort_decisions();
        if let Some(timer) = self.cleanup_timer.take() {
            timer.abort();
        }

        for task in self.tasks.values_mut() {
            task.workers.shutdown().await;
        }
        self.tasks.clear();
        self.finish_acquires().await;

        // Release is best effort and never fails the run, but the run should not
        // report back before the environments are actually gone.
        let mut releases = Vec::new();
        while let Some(handle) = self.releases.last_mut() {
            match handle.await {
                Ok(report) => {
                    if !report.is_clean() {
                        tracing::warn!(
                            problem_count = report.problems.len(),
                            released_count = report.released.len(),
                            kept_count = report.kept.len(),
                            "environment release reported problems"
                        );
                    }
                    releases.push(report);
                }
                Err(error) => tracing::warn!(error = ?error, "environment release task panicked"),
            }
            self.releases.pop();
        }

        // Every record has been handed over; what remains is the observers'
        // own queues and files.
        let mut observer_errors = Vec::new();
        for observer in &self.observers {
            if let Err(error) = observer.finish().await {
                tracing::warn!(error = ?error, "observer finish failed");
                observer_errors.push(error);
            }
        }

        // Per-run services come down before the report: teardown can be real
        // work — a listener thread joining — so it is awaited here instead of
        // blocking a worker from the guards' `Drop`.
        if !self.run_guards.is_empty() {
            let guards = mem::take(&mut self.run_guards);
            self.guard_teardown = Some(tokio::spawn(async move {
                for guard in guards {
                    guard.teardown().await;
                }
            }));
            if let Some(teardown) = self.guard_teardown.as_mut() {
                let _ = teardown.await;
            }
            self.guard_teardown.take();
        }

        // The run's own failures, which no caller reads: reported once here,
        // from the driver, so replay never emits them a second time.
        for error in self.engine.errors() {
            tracing::warn!(error = ?error, "engine run error");
        }

        let status = self.engine.folded_status();
        tracing::info!(
            status = %status,
            firing_count = self.engine.history().len(),
            engine_error_count = self.engine.errors().len(),
            released_count = releases.len(),
            observer_error_count = observer_errors.len(),
            "run finished"
        );

        let exit = self
            .engine
            .exit()
            .cloned()
            .unwrap_or(EngineExit::Terminal { status });
        ExecutionReport {
            exit,
            status,
            state: mem::replace(&mut self.engine, EngineState::new(Graph::new())),
            releases,
            observer_errors,
        }
    }

    /// A handle for cancelling this run from elsewhere.
    pub fn handle(&self) -> RunHandle {
        RunHandle {
            tx: self.tx.clone(),
        }
    }

    async fn on_signal(&mut self, signal: Signal) {
        match signal {
            Signal::CancelGroup { node, ack } => {
                let target = self
                    .engine
                    .graph()
                    .nodes
                    .iter()
                    .find(|candidate| candidate.name == node && candidate.cancel_group.is_some())
                    .map(|node| node.id);
                if let Some(node) = target {
                    self.feed(Event::CancelGroupRequested { node });
                }
                let _ = ack.send(target.is_some());
            }
            Signal::Inject(Event::CancelRequested { scope })
                if scope == ir::CancelScopeId::ROOT =>
            {
                self.on_root_cancel();
            }
            Signal::Inject(Event::KillRequested { scope }) if scope == ir::CancelScopeId::ROOT => {
                self.kill_root();
            }
            Signal::Inject(event) => self.feed(event),
            Signal::Deliver { firing, ctl, ack } => self.on_deliver(firing, ctl, ack),
            Signal::Progress { firing, event } => {
                let event = self.mask_progress(firing, event).await;
                self.note_question(firing, &event);
                self.feed(Event::StepProgress { firing, ev: event });
            }
            Signal::Finished {
                firing,
                attempt,
                outcome,
            } => {
                // A step that returned has a task; every driver-produced finish
                // (a dispatch failure, a resume settle) has none.
                let origin = if self.tasks.contains_key(&firing) {
                    ResultOrigin::Step
                } else {
                    ResultOrigin::Driver
                };
                self.finish(firing, attempt, outcome, origin).await;
            }
            Signal::Timeout {
                firing,
                attempt,
                timer,
            } => self.on_timeout(firing, attempt, timer),
            Signal::RetryDue {
                firing,
                next_attempt,
            } => {
                self.feed(Event::RetryElapsed {
                    firing,
                    next_attempt,
                });
            }
            Signal::Admitted {
                decision_id,
                resolution,
            } => {
                self.decision_tasks.remove(&decision_id);
                // A stop tier can clear a pending decision after its resolver
                // was asked; the moot answer is dropped, exactly as an aborted
                // resolver task's answer never arrives.
                if !self.engine.has_pending_admission(decision_id) {
                    return;
                }
                self.feed(Event::Admitted {
                    decision_id,
                    decision: resolution.decision,
                    trace: resolution.trace,
                });
            }
            Signal::RoutingResolved {
                decision_id,
                resolution,
            } => {
                self.decision_tasks.remove(&decision_id);
                if !self.engine.has_pending_routing(decision_id) {
                    return;
                }
                self.feed(Event::RoutingResolved {
                    decision_id,
                    groups: resolution.groups,
                });
            }
            Signal::HardDeadline { firing, attempt } => {
                self.on_hard_deadline(firing, attempt).await;
            }
            Signal::AcquireFinished { scope, id, result } => {
                self.on_acquire_finished(scope, id, result);
            }
            Signal::HookAdmitted {
                decision_id,
                decision,
            } => self.on_hook_admitted(decision_id, decision),
            Signal::ResultPrepared {
                firing,
                attempt,
                result,
            } => self.on_result_prepared(firing, attempt, result),
            Signal::HookedRoutingResolved {
                decision_id,
                resolution,
                notes,
            } => {
                self.decision_tasks.remove(&decision_id);
                if !self.engine.has_pending_routing(decision_id) {
                    return;
                }
                if let DecisionId::Route { firing, .. } = decision_id {
                    self.record_notes(firing, &notes);
                }
                self.feed(Event::RoutingResolved {
                    decision_id,
                    groups: resolution.groups,
                });
            }
        }
    }

    // ── Awaited extension points ─────────────────────────────────────────────

    /// The branch roles over the graph as it stands now.
    fn branch_map(&mut self) -> BranchMap {
        if !self.branches.covers(self.engine.graph()) {
            self.branches = BranchMap::of(self.engine.graph());
        }
        self.branches.clone()
    }

    /// Append the host's notes as progress records on `firing`, masked, in
    /// order, ahead of the record they annotate.
    fn record_notes(&mut self, firing: FiringId, notes: &[Note]) {
        for note in notes {
            let StepEvent::Custom(value) = note.to_step_event() else {
                continue;
            };
            let ev = StepEvent::Custom(self.sink.mask_value(&value));
            let commands = self.apply_event(Event::StepProgress { firing, ev });
            debug_assert!(commands.is_empty(), "a progress record derives no commands");
        }
    }

    /// Ask the host before an attempt. `true` when a hook took the decision;
    /// `false` when the caller should go straight to the resolver.
    fn hook_admission(&mut self, decision_id: DecisionId) -> bool {
        let Some(hooks) = self.hooks.clone() else {
            return false;
        };
        let DecisionId::AttemptStart { firing, .. } = decision_id else {
            return false;
        };
        let config = self
            .engine
            .pending_admissions()
            .find(|(id, _)| *id == decision_id)
            .and_then(|(_, pending)| pending.resolved.as_ref())
            .map(|resolved| resolved.config().clone());
        let branches = self.branch_map();
        let Some(view) = live_view(&self.engine, &branches, firing, config) else {
            return false;
        };
        let request = AdmitAttempt {
            decision: decision_id,
            view:     Arc::new(view),
        };
        let tx = self.tx.clone();
        let task = self.background.spawn(async move {
            let decision = hooks.before_attempt(request).await;
            let _ = tx
                .send(Signal::HookAdmitted {
                    decision_id,
                    decision,
                })
                .await;
        });
        self.track_decision(decision_id, task);
        true
    }

    fn on_hook_admitted(&mut self, decision_id: DecisionId, decision: AttemptDecision) {
        self.decision_tasks.remove(&decision_id);
        // A stop tier settled the firing while the host was deciding.
        if !self.engine.has_pending_admission(decision_id) {
            return;
        }
        let DecisionId::AttemptStart { firing, .. } = decision_id else {
            return;
        };
        self.record_notes(firing, &decision.notes);
        match decision.admission {
            Admission::Admit => self.resolve_admission(AdmitRequest { decision_id }),
            decision => self.feed(Event::Admitted {
                decision_id,
                decision,
                trace: vec![engine::MiddlewareKey::new("host.before_attempt")],
            }),
        }
    }

    /// Hand a finished attempt to the host before its record, when hooks are
    /// installed. `true` when the finish is now the hook task's to complete.
    fn hook_result(
        &mut self,
        firing: FiringId,
        attempt: Attempt,
        outcome: &Outcome,
        origin: ResultOrigin,
    ) -> bool {
        let Some(hooks) = self.hooks.clone() else {
            return false;
        };
        let branches = self.branch_map();
        let Some(view) = live_view(&self.engine, &branches, firing, None) else {
            return false;
        };
        let retry = &view.node.retry;
        let will_retry = retry.should_retry(&outcome.status) && retry.has_attempt_after(attempt);
        let exhausted = !retry.has_attempt_after(attempt);
        let request = PrepareResult {
            view: Arc::new(view),
            outcome: outcome.clone(),
            origin,
            will_retry,
            exhausted,
        };
        let tx = self.tx.clone();
        let task = self.background.spawn(async move {
            let result = hooks.prepare_result(request).await;
            let _ = tx
                .send(Signal::ResultPrepared {
                    firing,
                    attempt,
                    result,
                })
                .await;
        });
        self.preparing
            .insert(firing, (attempt, outcome.clone(), task));
        true
    }

    fn on_result_prepared(
        &mut self,
        firing: FiringId,
        attempt: Attempt,
        result: Result<Prepared, PrepareError>,
    ) {
        let Some((pending_attempt, original, _)) = self.preparing.remove(&firing) else {
            return;
        };
        if pending_attempt != attempt {
            return;
        }
        let mut outcome = original.clone();
        let mut notes = Vec::new();
        match result {
            Ok(prepared) => {
                let reason = prepared.adjustment.reason.clone();
                let changed = prepared.adjustment.apply(&mut outcome);
                notes.extend(prepared.notes);
                if changed {
                    notes.push(result_prepared_note(attempt, &original, &outcome, reason));
                }
            }
            Err(error) if error.fatal => {
                outcome = Outcome::new(
                    Status::Failure(
                        FailureInfo::new(error.message.clone())
                            .with_class(RESULT_PREPARATION_CLASS),
                    ),
                    original.output.clone(),
                );
                notes.push(result_prepared_note(
                    attempt,
                    &original,
                    &outcome,
                    Some(error.message),
                ));
            }
            Err(error) => {
                notes.push(Note::new(
                    RESULT_PREPARED_KIND,
                    serde_json::json!({
                        "attempt": attempt,
                        "best_effort_problem": error.message,
                    }),
                ));
            }
        }
        // The host's changes are masked like the step's own output.
        outcome.output = self.sink.mask_value(&outcome.output);
        outcome.status = outcome
            .status
            .map_messages(|message| self.sink.masker().mask(&message));
        self.record_notes(firing, &notes);
        self.feed(Event::StepFinished {
            firing,
            attempt,
            outcome,
        });
    }

    /// A kill ends the host's result preparation: every reported outcome is
    /// recorded as it was, so the run can reach quiescence.
    fn abandon_preparation(&mut self) {
        let preparing: Vec<(FiringId, (Attempt, Outcome, AbortHandle))> =
            self.preparing.drain().collect();
        for (firing, (attempt, outcome, task)) in preparing {
            task.abort();
            self.feed(Event::StepFinished {
                firing,
                attempt,
                outcome,
            });
        }
    }

    /// Resolve routing through `after_record`, the resolver, and
    /// `transition`, in that order, on one task. `true` when hooks own it.
    fn hook_routing(&mut self, request: RoutingRequest) -> Option<RoutingRequest> {
        let Some(hooks) = self.hooks.clone() else {
            return Some(request);
        };
        let DecisionId::Route { firing, .. } = request.decision_id else {
            return Some(request);
        };
        let branches = self.branch_map();
        let Some(view) = routing_view(&self.engine, &branches, firing) else {
            return Some(request);
        };
        let outcome = self
            .engine
            .pending_routings()
            .find(|pending| pending.firing == firing)
            .map_or_else(
                || Outcome::new(Status::Skipped, Value::Null),
                |pending| pending.outcome.clone(),
            );
        let view = Arc::new(view);
        let decision_id = request.decision_id;
        let resolver = self.decisions.clone();
        let tx = self.tx.clone();
        let task = self.background.spawn(async move {
            let mut notes = hooks
                .after_record(Recorded {
                    view:    view.clone(),
                    outcome: outcome.clone(),
                })
                .await;
            let group_ids: Vec<u32> = request.groups.iter().map(|group| group.group).collect();
            let resolution = match resolver.route_now(&request) {
                Some(resolution) => resolution,
                None => resolver
                    .route(request)
                    .await
                    .unwrap_or_else(|error| blocked_routing(&group_ids, error.message())),
            };
            let mut groups = resolution.groups;
            let attempt = view.attempt;
            let report = hooks
                .transition(Transition {
                    decision: decision_id,
                    view,
                    outcome,
                    groups: groups.clone(),
                })
                .await;
            apply_transition(&mut groups, &report);
            let transition = match &report {
                Ok(report) => {
                    notes.extend(report.notes.iter().cloned());
                    (!report.problems.is_empty() || !report.overrides.is_empty()).then(|| {
                        TransitionNote {
                            attempt,
                            problems: report.problems.clone(),
                            overrides: report.overrides.clone(),
                            blocked: None,
                        }
                    })
                }
                Err(error) => Some(TransitionNote {
                    attempt,
                    problems: Vec::new(),
                    overrides: Vec::new(),
                    blocked: Some(error.message.clone()),
                }),
            };
            if let Some(transition) = transition {
                notes.push(Note::new(
                    TRANSITION_KIND,
                    serde_json::to_value(transition).unwrap_or(Value::Null),
                ));
            }
            let _ = tx
                .send(Signal::HookedRoutingResolved {
                    decision_id,
                    resolution: RoutingResolution { groups },
                    notes,
                })
                .await;
        });
        self.track_decision(decision_id, task);
        None
    }

    /// The two-tier stop wiring (§10). The first root cancel feeds
    /// `CancelRequested` and arms the cleanup-grace timer; expiry, or another
    /// root cancel (a CLI maps a second Ctrl-C to it), feeds `KillRequested`.
    /// Both are ordinary External events, so the hard stop is in the log and
    /// replay reproduces it. The engine's own state says which tier this is —
    /// `is_cancelled` is set by exactly the root cancel and the root kill — so
    /// the driver keeps no count of its own.
    fn on_root_cancel(&mut self) {
        if self.engine.is_cancelled() {
            self.kill_root();
            return;
        }
        self.feed(Event::CancelRequested {
            scope: ir::CancelScopeId::ROOT,
        });
        self.arm_cleanup_timer();
    }

    fn arm_cleanup_timer(&mut self) {
        let grace = self.config.cleanup_grace;
        tracing::info!(
            live_firing_count = self.tasks.len(),
            cleanup_grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
            "cleanup grace timer armed"
        );
        let tx = self.tx.clone();
        self.cleanup_timer = Some(self.background.spawn(async move {
            time::sleep(grace).await;
            let _ = tx
                .send(Signal::Inject(Event::KillRequested {
                    scope: ir::CancelScopeId::ROOT,
                }))
                .await;
        }));
    }

    fn kill_root(&mut self) {
        if let Some(timer) = self.cleanup_timer.take() {
            timer.abort();
        }
        self.abort_decisions();
        tracing::warn!(live_firing_count = self.tasks.len(), "run kill requested");
        self.feed(Event::KillRequested {
            scope: ir::CancelScopeId::ROOT,
        });
        self.abandon_preparation();
    }

    fn abort_decisions(&mut self) {
        for (_, task) in self.decision_tasks.drain() {
            task.abort();
        }
    }

    fn track_decision(&mut self, decision_id: DecisionId, task: AbortHandle) {
        if let Some(previous) = self.decision_tasks.insert(decision_id, task) {
            previous.abort();
        }
    }

    /// Queue a synchronously resolved decision on the signal channel. A full
    /// channel falls back to a task that awaits capacity; decisions are
    /// validated by id, so relative order between them carries no meaning.
    fn send_decision_signal(&mut self, signal: Signal) {
        if let Err(mpsc::error::TrySendError::Full(signal)) = self.tx.try_send(signal) {
            let tx = self.tx.clone();
            self.background.spawn(async move {
                let _ = tx.send(signal).await;
            });
        }
    }

    /// Append-then-apply, then dispatch whatever the core asked for.
    ///
    /// The append happens inside `apply`, which records this event as
    /// `External` and everything it derives as `Core`.
    fn feed(&mut self, event: Event) {
        for command in self.apply_event(event) {
            self.dispatch(command);
        }
    }

    /// Run one event through the core: append, apply, hand back the commands.
    ///
    /// The one hook point for observers: every path into the engine — `feed`,
    /// `on_deliver`, everything — goes through here, so observers see every
    /// appended record exactly once, in seq order, with the post-apply state.
    fn apply_event(&mut self, event: Event) -> Vec<Command> {
        let before = self.engine.log.len();
        let state = mem::replace(&mut self.engine, EngineState::new(Graph::new()));
        let (state, commands) = apply(state, event);
        self.engine = state;
        self.notify_observers(before);
        commands
    }

    /// Hand every record appended since `from` to each observer.
    fn notify_observers(&self, from: usize) {
        if self.observers.is_empty() {
            return;
        }
        for record in &self.engine.log.records()[from..] {
            for observer in &self.observers {
                observer.on_record(record, &self.engine);
            }
        }
    }

    /// A host delivery: feed `ControlRequested` and report the disposition.
    ///
    /// The command-or-no-command result of `apply` is the disposition:
    /// `ControlRequested` yields at most one command, a `DeliverControl`
    /// carrying the `Deliver` (see `on_control_requested`). When it comes
    /// out, the ack travels with the forward and the forwarder completes
    /// it; when none does — the firing is dead, unknown, cancelling or
    /// awaiting a retry — the event is in the log regardless (the audit
    /// trail) and the host hears `NotLive`.
    fn on_deliver(&mut self, firing: FiringId, ctl: Control, ack: DeliverAck) {
        if self
            .engine
            .has_pending_admission(DecisionId::ExecutionStart)
        {
            self.early_deliveries
                .entry(firing)
                .or_default()
                .push((ctl, ack));
            return;
        }
        let commands = self.apply_event(Event::ControlRequested { firing, ctl });
        let command = commands.into_iter().next();
        let accepted = matches!(
            command,
            Some(Command::DeliverControl {
                ctl: Control::Deliver(_),
                ..
            })
        );
        tracing::debug!(
            firing = firing.raw(),
            accepted,
            "control delivery attempted"
        );
        match command {
            Some(Command::DeliverControl {
                firing,
                ctl: Control::Deliver(payload),
            }) => self.forward_deliver(firing, payload, Some(ack)),
            _ => {
                let _ = ack.send(DeliverDisposition::NotLive);
            }
        }
    }

    /// The resolver's admission, after any host hook admitted.
    fn resolve_admission(&mut self, request: AdmitRequest) {
        let decision_id = request.decision_id;
        if let Some(resolution) = self.decisions.admit_now(&request) {
            self.send_decision_signal(Signal::Admitted {
                decision_id,
                resolution,
            });
            return;
        }
        let resolver = self.decisions.clone();
        let tx = self.tx.clone();
        let task = self.background.spawn(async move {
            let resolution =
                resolver
                    .admit(request)
                    .await
                    .unwrap_or_else(|error| AdmissionResolution {
                        decision: Admission::Block {
                            reason: SmolStr::new(error.message()),
                        },
                        trace:    Vec::new(),
                    });
            let _ = tx
                .send(Signal::Admitted {
                    decision_id,
                    resolution,
                })
                .await;
        });
        self.track_decision(decision_id, task);
    }

    fn dispatch(&mut self, command: Command) {
        match command {
            Command::Admit { decision_id } => {
                if !self.hook_admission(decision_id) {
                    self.resolve_admission(AdmitRequest { decision_id });
                }
            }
            Command::ResolveRouting {
                decision_id,
                restart_allowed,
                groups,
            } => {
                let request = RoutingRequest {
                    decision_id,
                    restart_allowed,
                    groups,
                };
                let Some(request) = self.hook_routing(request) else {
                    return;
                };
                if let Some(resolution) = self.decisions.route_now(&request) {
                    self.send_decision_signal(Signal::RoutingResolved {
                        decision_id,
                        resolution,
                    });
                    return;
                }
                let resolver = self.decisions.clone();
                let tx = self.tx.clone();
                let task = self.background.spawn(async move {
                    let group_ids: Vec<u32> =
                        request.groups.iter().map(|group| group.group).collect();
                    let resolution = resolver
                        .route(request)
                        .await
                        .unwrap_or_else(|error| blocked_routing(&group_ids, error.message()));
                    let _ = tx
                        .send(Signal::RoutingResolved {
                            decision_id,
                            resolution,
                        })
                        .await;
                });
                self.track_decision(decision_id, task);
            }
            Command::AcquireScope { scope } => self.acquire(scope),
            Command::ReleaseScope { scope } => self.release(scope),
            Command::StartStep(resolved) => {
                let scope = resolved.scope();
                if self.acquires.contains_key(&scope) {
                    self.pending_starts.entry(scope).or_default().push(resolved);
                    return;
                }
                self.dispatch_start(&resolved);
            }
            Command::DeliverControl { firing, ctl } => match ctl {
                // A delivered value only forwards: no deadline, no reason — it never
                // starts the cancellation ladder or the kill tier.
                Control::Deliver(payload) => self.forward_deliver(firing, payload, None),
                stop => self.stop_step(firing, stop, CancelReason::Requested),
            },
            Command::ScheduleRetry {
                firing,
                next_attempt,
                base_delay,
            } => {
                let delay = jittered(base_delay, firing, next_attempt);
                tracing::debug!(
                    firing = firing.raw(),
                    next_attempt = next_attempt.raw(),
                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    "retry scheduled"
                );
                let tx = self.tx.clone();
                self.background.spawn(async move {
                    time::sleep(delay).await;
                    let _ = tx
                        .send(Signal::RetryDue {
                            firing,
                            next_attempt,
                        })
                        .await;
                });
            }
            // The core resolves expansion itself, and the run ends when the loop
            // sees the state finished.
            Command::ExpandNode { .. } | Command::FinishExecution { .. } => {}
        }
    }

    fn dispatch_start(&mut self, resolved: &ResolvedFiring) {
        let (firing, attempt) = (resolved.id(), resolved.attempt());
        let live = self.engine.firing(firing);
        // A firing the stop tiers marked cancelling reaches dispatch only on
        // resume, and is never re-spawned: it finishes directly with the same
        // `Cancelled` outcome the live run would have fed once the stop landed,
        // tagged with the tier from the replayed state (§10).
        if let Some(node) = live
            .filter(|state| state.cancelling)
            .map(|state| state.node)
        {
            self.reject_pending_forwards(firing);
            self.pending_failures.remove(&firing);
            self.finish_instead_of_resuming(firing, attempt, node);
            return;
        }
        // A started firing is already acknowledged in the loaded log; a second
        // `StepStarted` would be a replay divergence.
        let started = live.is_some_and(|state| state.started);
        if !started {
            // The acknowledgement that the attempt was dispatched. It is applied
            // here, before `start` can spawn the runner, so the log places it
            // ahead of anything the runner sends back: an instant runner's
            // `Finished` riding the signal channel can otherwise land first, and
            // the late `StepStarted` then reports the finalized firing as unknown.
            let commands = self.apply_event(Event::StepStarted { firing, attempt });
            debug_assert!(commands.is_empty(), "StepStarted derives no commands");
        }
        if let Some((attempt, message)) = self.pending_failures.remove(&firing) {
            self.reject_pending_forwards(firing);
            self.fail_now(firing, attempt, &message, steps::SECRET_UNAVAILABLE_CLASS);
            return;
        }
        self.start(resolved);
        self.flush_early_deliveries(firing);
        self.flush_pending_forwards(firing);
    }

    /// Finish a cancelling firing that resume would otherwise re-spawn: one
    /// code path for both tiers, the tier read from the replayed state. The
    /// finish rides the signal channel like every step result, so its place in
    /// the log is its arrival order.
    fn finish_instead_of_resuming(&mut self, firing: FiringId, attempt: Attempt, node: NodeId) {
        let escalation = if self.engine.is_node_killed(node) {
            &KILLED_BEFORE_RESUME
        } else {
            &CANCELLED_BEFORE_RESUME
        };
        let outcome = escalation_outcome(Status::Cancelled, escalation);
        let tx = self.tx.clone();
        self.background.spawn(async move {
            let _ = tx
                .send(Signal::Finished {
                    firing,
                    attempt,
                    outcome,
                })
                .await;
        });
    }

    // ── Scopes ─────────────────────────────────────────────────────────────

    fn acquire(&mut self, scope: ScopeId) {
        if self.envs.contains_key(&scope) || self.acquires.contains_key(&scope) {
            return;
        }
        let mut spec = self.scope_spec(scope);
        let mut ctx = AcquireContext::new(self.secrets.clone(), self.progress.clone());
        let leases = self.config.scope_leases.clone();
        let identity = self.engine.scope_identity(scope);
        let inherited_mismatch = self
            .config
            .runtime_override
            .as_ref()
            .zip(self.engine.graph().scope(scope))
            .is_some_and(|(inherited, declared)| {
                matches!(declared.runtime.target, ir::RuntimeTarget::Container { .. })
                    && declared.runtime.target != inherited.target
            });
        let executor = self.executor.clone();
        let abandoned = self.abandoned_tx.clone();
        let tx = self.tx.clone();
        let id = self.next_acquire_id;
        self.next_acquire_id = self
            .next_acquire_id
            .checked_add(1)
            .expect("a run cannot start 2^64 scope acquisitions");
        let join = self.background.spawn(async move {
            let result = async {
                if inherited_mismatch {
                    return Err(EnvError::backend(
                        "coordinator",
                        "acquire inherited scope",
                        "scope declares a different container from its inherited sandbox",
                    ));
                }
                match leases {
                    ScopeLeases::None => {}
                    ScopeLeases::Shared(lease) => ctx = ctx.with_lease(lease),
                    ScopeLeases::Owned(allocator) => {
                        let identity = identity.ok_or_else(|| {
                            EnvError::backend(
                                "coordinator",
                                "reserve lease",
                                "scope has no declared identity",
                            )
                        })?;
                        let assignment = allocator.reserve(identity, &spec).await?;
                        ctx = ctx.with_lease(assignment.lease);
                        spec.workspace_id = assignment.workspace;
                    }
                }
                executor
                    .acquire(&spec, &ctx)
                    .await
                    .map(|handle| AcquiredScope::new(handle, abandoned))
            }
            .await
            // The deliberate render point: the failure becomes
            // `FailureInfo.message`, a rendered projection, so the whole
            // source chain is flattened into it here.
            .map_err(|error| render_chain(&error));
            let _ = tx.send(Signal::AcquireFinished { scope, id, result }).await;
        });
        self.acquires.insert(scope, ScopeAcquire { id, join });
    }

    fn on_acquire_finished(
        &mut self,
        scope: ScopeId,
        id: u64,
        result: Result<AcquiredScope, String>,
    ) {
        let current = self
            .acquires
            .get(&scope)
            .is_some_and(|acquire| acquire.id == id);
        if !current {
            return;
        }
        self.acquires.remove(&scope);
        match result {
            Ok(acquired) => {
                self.envs.insert(scope, acquired.into_handle());
            }
            Err(error) => {
                // Not a run abort: every firing in this scope fails, routably.
                // The message arrives already rendered, chain and all, from
                // the acquire task's render point.
                self.acquire_failures.insert(scope, error);
            }
        }

        for resolved in self.pending_starts.remove(&scope).unwrap_or_default() {
            self.dispatch_start(&resolved);
        }
    }

    fn release(&mut self, scope: ScopeId) {
        if let Some(acquire) = self.acquires.remove(&scope) {
            acquire.join.abort();
        }
        if let Some(handle) = self.envs.remove(&scope) {
            self.release_handle(scope, handle);
        }
    }

    fn release_handle(&mut self, scope: ScopeId, handle: EnvHandle) {
        let outcome = if self.scope_failed.contains(&scope) {
            ScopeOutcome::Failed
        } else {
            ScopeOutcome::Succeeded
        };
        self.spawn_release(handle, outcome);
    }

    fn spawn_release(&mut self, handle: EnvHandle, outcome: ScopeOutcome) {
        let executor = self.executor.clone();
        self.releases.push(tokio::spawn(async move {
            executor.release(handle, outcome).await
        }));
    }

    /// Stop unfinished acquires, then collect any successful result that raced
    /// with scope release. Waiting for the aborted tasks guarantees no acquire
    /// can send another result after the channel is drained.
    async fn finish_acquires(&mut self) {
        self.background.shutdown().await;
        self.acquires.clear();
        while let Ok(signal) = self.rx.try_recv() {
            if let Signal::AcquireFinished {
                scope,
                result: Ok(acquired),
                ..
            } = signal
            {
                self.release_handle(scope, acquired.into_handle());
            }
        }
        while let Ok(handle) = self.abandoned_rx.try_recv() {
            self.spawn_release(handle, ScopeOutcome::Failed);
        }
    }

    fn scope_spec(&self, scope: ScopeId) -> ScopeSpec {
        let environment = EnvironmentId::scoped(self.config.environment_prefix.as_deref(), scope);
        let workspace =
            self.config.workspace_override.clone().unwrap_or_else(|| {
                WorkspaceId::scoped(self.config.workspace_prefix.as_deref(), scope)
            });
        let mut spec = ScopeSpec::new(scope, environment.as_str())
            .with_environment_id(environment)
            .with_workspace_id(workspace)
            .with_grace(self.config.grace);
        let Some(definition) = self.engine.graph().scope(scope) else {
            return self.override_runtime(spec);
        };
        // Scope env reads parameters and the execution's initial context.
        // Later context updates and firing-local values cannot change it.
        let mut initial_context = RunContext::new();
        initial_context.merge(&self.start.context);
        let mut params_only = StaticCtx::new();
        for (key, value) in &self.engine.graph().params {
            params_only.set(key, value.clone());
        }
        let env_context = EvalEnv::new(&Value::Null, &initial_context, &params_only);
        let resolve = |entries: &BTreeMap<SmolStr, ExprOrValue>| -> BTreeMap<SmolStr, SmolStr> {
            let mut env: BTreeMap<SmolStr, SmolStr> = BTreeMap::new();
            for (key, value) in entries {
                let resolved = match value {
                    ExprOrValue::Value(v) => value_to_string(v),
                    ExprOrValue::Expr(id) => {
                        match eval(&self.engine.graph().exprs, *id, &env_context) {
                            Ok(v) => value_to_string(&v),
                            Err(_) => continue,
                        }
                    }
                };
                env.insert(key.clone(), SmolStr::new(resolved));
            }
            env
        };
        let services = definition
            .services
            .iter()
            .map(|service| executor::ServiceSpec {
                name:        service.name.clone(),
                image:       service.image.clone(),
                env:         resolve(&service.env),
                options:     service.options.clone(),
                credentials: service.credentials.clone(),
            })
            .collect();
        spec = spec
            .with_env(resolve(&definition.env))
            .with_runtime(definition.runtime.clone())
            .with_services(services);
        self.override_runtime(spec)
    }

    /// An inherited sandbox decides the runtime target, not the graph: the
    /// scope runs where its caller's scope runs.
    fn override_runtime(&self, spec: ScopeSpec) -> ScopeSpec {
        match &self.config.runtime_override {
            Some(runtime) => spec.with_runtime(runtime.clone()),
            None => spec,
        }
    }

    // ── Steps ──────────────────────────────────────────────────────────────

    fn start(&mut self, resolved: &ResolvedFiring) {
        let firing = resolved.id();
        let attempt = resolved.attempt();
        let scope = resolved.scope();
        let node = resolved.node();

        // An environment that could not be acquired fails its firings rather than
        // aborting the run: the failure routes like any other.
        if let Some(message) = self.acquire_failures.get(&scope).cloned() {
            self.scope_failed.insert(scope);
            let outcome = Outcome::new(
                Status::Failure(
                    FailureInfo::new(format!("could not acquire the environment: {message}"))
                        .with_class(EnvError::ACQUIRE_CLASS),
                ),
                Value::Null,
            );
            let tx = self.tx.clone();
            self.background.spawn(async move {
                let _ = tx
                    .send(Signal::Finished {
                        firing,
                        attempt,
                        outcome,
                    })
                    .await;
            });
            return;
        }

        let Some((env, container_runner)) = self
            .envs
            .get(&scope)
            .map(|handle| (handle.exec(), handle.container_runner()))
        else {
            self.fail_now(
                firing,
                attempt,
                "no environment was acquired for this scope",
                EnvError::ACQUIRE_CLASS,
            );
            return;
        };
        // One lookup for both: a firing whose node left the graph has no runner
        // either, and fails here rather than carrying a nameless step forward.
        let Some((name, kind, runner)) = self.engine.graph().node(node).and_then(|n| {
            let runner = self.runners.get(&n.step.kind)?;
            Some((n.name.clone(), n.step.kind.clone(), runner))
        }) else {
            self.fail_now(firing, attempt, "no runner for this step kind", NO_RUNNER);
            return;
        };

        let span = tracing::debug_span!(
            "driver.step",
            firing = firing.raw(),
            node = %name,
            node_id = node.raw(),
            attempt = attempt.raw(),
            generation = resolved.generation().raw(),
            scope = scope.raw(),
            step_kind = %kind,
        );

        let (log_tx, mut log_rx) = mpsc::channel::<StepEvent>(256);
        let (control_tx, control_rx) = mpsc::channel::<Control>(CONTROL_CHANNEL_CAPACITY);

        // The firing's serialized forwarder: the one place that awaits control
        // channel capacity, so backpressure never reaches the driver loop and
        // sends land in order. It drains what is queued when the task goes away —
        // a dropped receiver resolves the remaining acks `NotLive`.
        //
        // Unbounded, with a bound in practice: an acked caller awaits its ack
        // before sending again, so the queue holds at most one `Forward` per
        // concurrent host caller, plus the driver's own ack-less stop signals —
        // a handful per escalation, never per unit of work.
        let (forward_tx, mut forward_rx) = mpsc::unbounded_channel::<Forward>();
        let mut workers = JoinSet::new();
        workers.spawn(async move {
            while let Some(forward) = forward_rx.recv().await {
                let disposition = match control_tx.send(forward.ctl).await {
                    Ok(()) => DeliverDisposition::Delivered,
                    Err(_) => DeliverDisposition::NotLive,
                };
                if let Some(ack) = forward.ack {
                    let _ = ack.send(disposition);
                }
            }
        });

        let progress_tx = self.tx.clone();
        let (runner_returned, mut close_logs) = oneshot::channel();
        let (logs_drained, drained) = oneshot::channel();
        workers.spawn(async move {
            loop {
                let event = tokio::select! {
                    event = log_rx.recv() => event,
                    _ = &mut close_logs, if !log_rx.is_closed() => {
                        // The runner can leave log sender clones in host
                        // services. Stop accepting new output, but preserve
                        // every event it queued before returning.
                        log_rx.close();
                        continue;
                    }
                };
                let Some(event) = event else {
                    break;
                };
                if progress_tx
                    .send(Signal::Progress { firing, event })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let _ = logs_drained.send(());
        });

        let ctx = StepCtx {
            firing,
            attempt,
            scope,
            node: name.clone(),
            config: resolved.config().clone(),
            env,
            runner: container_runner,
            secrets: self.secrets.clone(),
            caps: self.caps.clone(),
            logs: log_tx,
            control: control_rx,
        };
        let done_tx = self.tx.clone();
        workers.spawn(
            async move {
                let outcome = runner.run(ctx).await;
                let _ = runner_returned.send(());
                let _ = drained.await;
                let _ = done_tx
                    .send(Signal::Finished {
                        firing,
                        attempt,
                        outcome,
                    })
                    .await;
            }
            .instrument(span.clone()),
        );

        // The per-attempt timeout. `Budget.timeout` is per attempt, not per
        // firing, and only an `ExecutorEnforced` node gets the driver's timer:
        // a `HandlerManaged` step consumes its timeout itself, and a second
        // wrapper around it would time out work the step already bounds.
        let budget = self
            .engine
            .graph()
            .node(node)
            .map(|n| n.budget)
            .filter(|budget| {
                !budget.timeout.is_zero()
                    && budget.timeout_policy == ir::TimeoutPolicy::ExecutorEnforced
            })
            .map(|budget| AttemptBudget {
                remaining: budget.timeout,
                armed_at:  None,
                timer:     None,
                timer_id:  0,
                pending:   BTreeSet::new(),
            });

        self.tasks.insert(firing, Task {
            name,
            scope,
            attempt,
            started: time::Instant::now(),
            span,
            forwards: forward_tx,
            workers,
            deadline: None,
            reason: None,
            budget,
        });
        self.arm_budget(firing);
    }

    /// Start (or restart) the attempt timer with whatever budget remains.
    fn arm_budget(&mut self, firing: FiringId) {
        let Some(task) = self.tasks.get_mut(&firing) else {
            return;
        };
        let attempt = task.attempt;
        let Some(budget) = task.budget.as_mut() else {
            return;
        };
        if budget.armed_at.is_some() || !budget.pending.is_empty() {
            return;
        }
        self.next_timer_id += 1;
        let timer_id = self.next_timer_id;
        budget.timer_id = timer_id;
        budget.armed_at = Some(time::Instant::now());
        let remaining = budget.remaining;
        let tx = self.tx.clone();
        budget.timer = Some(task.workers.spawn(async move {
            time::sleep(remaining).await;
            let _ = tx
                .send(Signal::Timeout {
                    firing,
                    attempt,
                    timer: timer_id,
                })
                .await;
        }));
        tracing::debug!(
            parent: &task.span,
            firing = firing.raw(),
            attempt = attempt.raw(),
            remaining_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
            "attempt timer armed"
        );
    }

    /// Stop the attempt timer and bank the time it had left.
    fn pause_budget(task: &mut Task) {
        let Task {
            span,
            attempt,
            budget,
            ..
        } = task;
        let Some(budget) = budget.as_mut() else {
            return;
        };
        let Some(armed_at) = budget.armed_at.take() else {
            return;
        };
        if let Some(timer) = budget.timer.take() {
            timer.abort();
        }
        budget.remaining = budget.remaining.saturating_sub(armed_at.elapsed());
        tracing::debug!(
            parent: &*span,
            attempt = attempt.raw(),
            remaining_ms = u64::try_from(budget.remaining.as_millis()).unwrap_or(u64::MAX),
            pending_questions = budget.pending.len(),
            "attempt timer paused"
        );
    }

    /// A step asked the host a question: an interaction wait begins for this
    /// firing and attempt, and an executor-enforced budget stops counting.
    /// A repeated question id while the first is pending is one wait.
    fn note_question(&mut self, firing: FiringId, event: &StepEvent) {
        let Some(question) = steps::Question::from_event(event) else {
            return;
        };
        let Some(task) = self.tasks.get_mut(&firing) else {
            return;
        };
        tracing::info!(
            parent: &task.span,
            firing = firing.raw(),
            attempt = task.attempt.raw(),
            question = %question.id,
            "interaction wait started"
        );
        if let Some(budget) = task.budget.as_mut() {
            budget.pending.insert(question.id);
            Self::pause_budget(task);
        }
    }

    /// A host answered one of a step's questions: the wait ends, and once no
    /// question of this attempt is pending the budget resumes.
    fn note_answer(&mut self, firing: FiringId, payload: &Value) {
        let Some(answer) = steps::Answer::from_value(payload) else {
            return;
        };
        let Some(id) = answer.question.as_deref() else {
            return;
        };
        let Some(task) = self.tasks.get_mut(&firing) else {
            return;
        };
        let Task {
            span,
            attempt,
            budget,
            ..
        } = task;
        let attempt = attempt.raw();
        let Some(budget) = budget.as_mut() else {
            tracing::info!(
                parent: &*span,
                firing = firing.raw(),
                attempt,
                question = id,
                "interaction wait ended"
            );
            return;
        };
        if !budget.pending.remove(id) {
            // An answer to a question this attempt never asked, or one already
            // answered: stale, and no wait to end.
            return;
        }
        tracing::info!(
            parent: &*span,
            firing = firing.raw(),
            attempt,
            question = id,
            pending_questions = budget.pending.len(),
            "interaction wait ended"
        );
        if budget.pending.is_empty() {
            self.arm_budget(firing);
        }
    }

    fn fail_now(&mut self, firing: FiringId, attempt: Attempt, message: &str, class: FailureClass) {
        tracing::warn!(
            firing = firing.raw(),
            attempt = attempt.raw(),
            failure_class = %class,
            "step could not be dispatched"
        );
        let outcome = Outcome::new(
            Status::Failure(FailureInfo::new(message).with_class(class)),
            Value::Null,
        );
        let tx = self.tx.clone();
        self.background.spawn(async move {
            let _ = tx
                .send(Signal::Finished {
                    firing,
                    attempt,
                    outcome,
                })
                .await;
        });
    }

    /// Tell a step to stop, and start the clock on how long it may take.
    ///
    /// The send itself rides the firing's forwarder — the control channel may
    /// be full of pending deliveries, and that must never block the driver
    /// loop — so the deadline is armed here, at signal time, not after the
    /// send lands: the deadline is what guarantees progress.
    fn stop_step(&mut self, firing: FiringId, ctl: Control, reason: CancelReason) {
        let Some(task) = self.tasks.get_mut(&firing) else {
            self.finish_pending_start(firing, &ctl);
            return;
        };
        // First reason wins: a timeout that beat a cancel makes this `TimedOut`.
        if task.reason.is_none() {
            task.reason = Some(reason);
        }
        let kill = matches!(ctl, Control::Kill);
        tracing::debug!(
            parent: &task.span,
            firing = firing.raw(),
            attempt = task.attempt.raw(),
            control = if kill { "kill" } else { "cancel" },
            reason = ?reason,
            "step stop signalled"
        );
        let _ = task.forwards.send(Forward { ctl, ack: None });

        // A kill's deadline has no grace in it: the step was told to SIGKILL and
        // return, so a deadline armed by an earlier polite cancel is re-armed
        // zero-slack.
        if kill && let Some(deadline) = task.deadline.take() {
            deadline.abort();
        }
        if task.deadline.is_none() {
            // No step kind, however buggy, may wedge a run.
            let limit = if kill {
                self.config.hard_deadline_slack
            } else {
                self.config.grace + self.config.hard_deadline_slack
            };
            let attempt = task.attempt;
            let tx = self.tx.clone();
            task.deadline = Some(task.workers.spawn(async move {
                time::sleep(limit).await;
                let _ = tx.send(Signal::HardDeadline { firing, attempt }).await;
            }));
        }
    }

    /// Settle a firing whose runner is still parked behind scope acquisition.
    /// The acquire stays alive until the core either admits cleanup in the same
    /// scope or releases the scope because nothing else needs it.
    fn finish_pending_start(&mut self, firing: FiringId, ctl: &Control) {
        let Some(scope) = self.engine.firing(firing).map(|state| state.scope) else {
            return;
        };
        let Some(pending) = self.pending_starts.get_mut(&scope) else {
            return;
        };
        let Some(index) = pending.iter().position(|resolved| resolved.id() == firing) else {
            return;
        };
        let resolved = pending.remove(index);
        if pending.is_empty() {
            self.pending_starts.remove(&scope);
        }
        self.reject_pending_forwards(firing);
        self.pending_failures.remove(&firing);

        tracing::debug!(
            firing = firing.raw(),
            attempt = resolved.attempt().raw(),
            scope = scope.raw(),
            control = if matches!(ctl, Control::Kill) {
                "kill"
            } else {
                "cancel"
            },
            "step cancelled while scope acquisition was pending"
        );
        let attempt = resolved.attempt();
        let outcome = Outcome::new(Status::Cancelled, Value::Null);
        let tx = self.tx.clone();
        self.background.spawn(async move {
            let _ = tx
                .send(Signal::Finished {
                    firing,
                    attempt,
                    outcome,
                })
                .await;
        });
    }

    /// Queue a `Deliver` on the firing's forwarder: no deadline, no reason.
    ///
    /// `$secret` references in the payload resolve here, at command-dispatch
    /// time — the logged event keeps the reference, so dynamic values never
    /// enter the log. An unresolvable reference — the post-resume shape,
    /// where the host must re-provide dynamic values — fails the step with
    /// `secret_unavailable`.
    fn forward_deliver(&mut self, firing: FiringId, payload: Value, ack: Option<DeliverAck>) {
        let payload = match resolve_secret_refs(payload, self.secrets.as_ref()) {
            Ok(payload) => payload,
            Err(missing) => {
                if let Some(task) = self.tasks.get(&firing) {
                    let attempt = task.attempt;
                    self.fail_now(
                        firing,
                        attempt,
                        &format!("secret `{missing}` is not available for delivery"),
                        steps::SECRET_UNAVAILABLE_CLASS,
                    );
                } else if self.is_pending_start(firing)
                    && let Some(attempt) = self.engine.firing(firing).map(|state| state.attempt)
                {
                    self.pending_failures.entry(firing).or_insert_with(|| {
                        (
                            attempt,
                            format!("secret `{missing}` is not available for delivery"),
                        )
                    });
                }
                if let Some(ack) = ack {
                    let _ = ack.send(DeliverDisposition::NotLive);
                }
                return;
            }
        };
        self.note_answer(firing, &payload);
        let forward = Forward {
            ctl: Control::Deliver(payload),
            ack,
        };
        let Some(task) = self.tasks.get(&firing) else {
            if self.is_pending_start(firing) {
                self.pending_forwards
                    .entry(firing)
                    .or_default()
                    .push(forward);
                return;
            }
            if let Some(ack) = forward.ack {
                let _ = ack.send(DeliverDisposition::NotLive);
            }
            return;
        };
        if let Err(rejected) = task.forwards.send(forward)
            && let Some(ack) = rejected.0.ack
        {
            let _ = ack.send(DeliverDisposition::NotLive);
        }
    }

    fn is_pending_start(&self, firing: FiringId) -> bool {
        self.pending_starts
            .values()
            .flatten()
            .any(|resolved| resolved.id() == firing)
    }

    fn flush_pending_forwards(&mut self, firing: FiringId) {
        let Some(forwards) = self.pending_forwards.remove(&firing) else {
            return;
        };
        let Some(task) = self.tasks.get(&firing) else {
            for forward in forwards {
                if let Some(ack) = forward.ack {
                    let _ = ack.send(DeliverDisposition::NotLive);
                }
            }
            return;
        };
        for forward in forwards {
            if let Err(rejected) = task.forwards.send(forward)
                && let Some(ack) = rejected.0.ack
            {
                let _ = ack.send(DeliverDisposition::NotLive);
            }
        }
    }

    fn flush_early_deliveries(&mut self, firing: FiringId) {
        let Some(deliveries) = self.early_deliveries.remove(&firing) else {
            return;
        };
        for (ctl, ack) in deliveries {
            self.on_deliver(firing, ctl, ack);
        }
    }

    fn reject_pending_forwards(&mut self, firing: FiringId) {
        let Some(forwards) = self.pending_forwards.remove(&firing) else {
            return;
        };
        for forward in forwards {
            if let Some(ack) = forward.ack {
                let _ = ack.send(DeliverDisposition::NotLive);
            }
        }
    }

    fn on_timeout(&mut self, firing: FiringId, attempt: Attempt, timer: u64) {
        let current = self.tasks.get(&firing).is_some_and(|task| {
            task.attempt == attempt
                && task
                    .budget
                    .as_ref()
                    .is_some_and(|budget| budget.armed_at.is_some() && budget.timer_id == timer)
        });
        if !current {
            // A timer paused or re-armed after this expiry was queued.
            return;
        }
        self.stop_step(firing, Control::Cancel, CancelReason::TimedOut);
    }

    async fn on_hard_deadline(&mut self, firing: FiringId, attempt: Attempt) {
        let Some(task) = self.tasks.get_mut(&firing) else {
            return;
        };
        if task.attempt != attempt {
            return;
        }
        let reason = task.reason.unwrap_or(CancelReason::Requested);
        let name = task.name.clone();
        tracing::warn!(
            parent: &task.span,
            firing = firing.raw(),
            attempt = attempt.raw(),
            reason = ?reason,
            "step did not return after cancel"
        );
        // The step ignored its cancel. Stop waiting for it.
        task.workers.abort_all();

        let status = match reason {
            CancelReason::TimedOut => Status::TimedOut,
            CancelReason::Requested => Status::Cancelled,
        };
        let _ = self
            .sink
            .record(
                &name,
                firing.raw(),
                ir::LogStream::Stderr,
                "the step did not return after Control::Cancel; the driver stopped waiting",
            )
            .await;
        self.finish(
            firing,
            attempt,
            escalation_outcome(status, &CANCEL_FORCED),
            ResultOrigin::Driver,
        )
        .await;
    }

    async fn finish(
        &mut self,
        firing: FiringId,
        attempt: Attempt,
        outcome: Outcome,
        origin: ResultOrigin,
    ) {
        // A firing finished from `fail_now` or from resume's direct finish has no
        // task, and so no step span: those events belong to the run instead.
        if let Some(task) = self.tasks.get_mut(&firing) {
            task.workers.shutdown().await;
        }
        let mut outcome = outcome;
        let (reason, span) = match self.tasks.remove(&firing) {
            Some(task) => {
                if outcome.status.is_failure() {
                    self.scope_failed.insert(task.scope);
                }
                // The observed wall-clock duration, when the step kind did
                // not report one of its own.
                if outcome.metrics.duration_ms.is_none() {
                    outcome.metrics.duration_ms =
                        Some(u64::try_from(task.started.elapsed().as_millis()).unwrap_or(u64::MAX));
                }
                (task.reason, task.span)
            }
            None => (None, tracing::Span::current()),
        };

        // A step reports `Cancelled` whichever way it was stopped; only the driver
        // knows a timer got there first.
        if reason == Some(CancelReason::TimedOut) && matches!(outcome.status, Status::Cancelled) {
            outcome.status = Status::TimedOut;
        }

        // The splice backstop, scoped to splice payloads: a registered secret
        // value inside a `SpliceRequest` fails the firing with no fragment
        // applied — never masked, because masking a fragment would change the
        // executable plan. The hit path is exact: the requests are cleared
        // before the append, so the offending request never reaches the log,
        // and the canonical `Failure{class: invalid_splice}` carries a
        // non-secret message. `context_updates` drop with the requests, per
        // the invalid-splice rule.
        if !outcome.splices.is_empty() {
            let masker = self.sink.masker();
            if !masker.is_empty()
                && masker.contains_secret(
                    &serde_json::to_string(&outcome.splices)
                        .expect("splice requests always encode"),
                )
            {
                tracing::warn!(
                    parent: &span,
                    firing = firing.raw(),
                    attempt = attempt.raw(),
                    splice_count = outcome.splices.len(),
                    "splice request held a registered secret"
                );
                outcome = engine::reject_splices(
                    outcome,
                    "a splice request contained a registered secret value; \
                     the request list was dropped before the log",
                );
            }
        }

        // Masking happens before the append, so the persisted log is post-mask.
        outcome.output = self.sink.mask_value(&outcome.output);
        outcome.context_updates = outcome
            .context_updates
            .iter()
            .map(|(k, v)| (k.clone(), self.sink.mask_value(v)))
            .collect();
        // Failure messages quote raw text — step stderr, unparseable
        // `$GITHUB_ENV` lines — so they are masked like the output is. The
        // class stays: routing matches on it, and classes carry no free text.
        outcome.status = outcome
            .status
            .map_messages(|message| self.sink.masker().mask(&message));
        outcome.metrics.custom = outcome
            .metrics
            .custom
            .iter()
            .map(|(k, v)| (k.clone(), self.sink.mask_value(v)))
            .collect();

        tracing::debug!(
            parent: &span,
            firing = firing.raw(),
            attempt = attempt.raw(),
            status = outcome.status.tag(),
            failure_class = outcome.status.failure_info().map(|info| info.class.as_str()),
            duration_ms = outcome.metrics.duration_ms,
            "step finished"
        );

        // The host prepares the result before its record when hooks are
        // installed; the hook task then feeds the finish.
        if self.hook_result(firing, attempt, &outcome, origin) {
            return;
        }
        self.feed(Event::StepFinished {
            firing,
            attempt,
            outcome,
        });
    }

    async fn mask_progress(&self, firing: FiringId, event: StepEvent) -> StepEvent {
        let name = self
            .tasks
            .get(&firing)
            .map_or_else(|| "step".to_string(), |t| t.name.to_string());
        match event {
            StepEvent::Log { stream, line } => {
                let line = self.sink.record(&name, firing.raw(), stream, &line).await;
                StepEvent::Log { stream, line }
            }
            StepEvent::Artifact { name, uri } => StepEvent::Artifact {
                name: SmolStr::new(self.sink.masker().mask(&name)),
                uri:  self.sink.masker().mask(&uri),
            },
            StepEvent::Custom(value) => StepEvent::Custom(self.sink.mask_value(&value)),
        }
    }
}

/// Every group blocked with one reason: what a failed resolver resolves to.
fn blocked_routing(groups: &[u32], reason: &str) -> RoutingResolution {
    RoutingResolution {
        groups: groups
            .iter()
            .map(|group| GroupDecision {
                group:    *group,
                draw:     None,
                trace:    Vec::new(),
                decision: RouteDecision::Block {
                    reason: SmolStr::new(reason),
                },
            })
            .collect(),
    }
}

/// The original-evidence note for an adjusted result.
fn result_prepared_note(
    attempt: Attempt,
    original: &Outcome,
    effective: &Outcome,
    reason: Option<String>,
) -> Note {
    Note::new(
        RESULT_PREPARED_KIND,
        serde_json::to_value(ResultPreparedNote {
            attempt,
            original: original.status.clone(),
            effective: effective.status.clone(),
            output: original.output.clone(),
            reason,
        })
        .unwrap_or(Value::Null),
    )
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// An error and its whole `source()` chain on one line, for the places that
/// must flatten a typed error into a recorded message.
fn render_chain(error: &dyn Error) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// Replace every `{"$secret": "NAME"}` reference in a `Deliver` payload — the
/// exact one-key form, anywhere in the value — with its resolved secret.
/// Returns the first unresolvable name. Resolution registers the value with the
/// masker, so anything resolvable is maskable by construction.
fn resolve_secret_refs(value: Value, secrets: &dyn SecretProvider) -> Result<Value, SmolStr> {
    match value {
        Value::Object(map) => {
            if map.len() == 1
                && let Some(name) = map.get(SECRET_REF_KEY)
                && let Some(name) = name.as_str()
            {
                return match secrets.resolve(name) {
                    Ok(secret) => Ok(Value::String(secret.expose().to_string())),
                    Err(_) => Err(SmolStr::new(name)),
                };
            }
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                out.insert(key, resolve_secret_refs(child, secrets)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .into_iter()
            .map(|item| resolve_secret_refs(item, secrets))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other),
    }
}

#[cfg(test)]
mod teardown_tests {
    use std::future::pending;
    use std::sync::mpsc as sync_mpsc;

    use executor::MapSecrets;
    use executor_sandbox::HostExecutor;
    use testkit::RunDir;
    use tokio::sync::Notify;

    use super::*;

    struct GatedRelease {
        host:    HostExecutor,
        started: Notify,
        gate:    Notify,
    }

    #[async_trait::async_trait]
    impl Executor for GatedRelease {
        async fn acquire(
            &self,
            spec: &ScopeSpec,
            ctx: &AcquireContext,
        ) -> Result<EnvHandle, EnvError> {
            self.host.acquire(spec, ctx).await
        }

        async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
            assert_eq!(
                outcome,
                ScopeOutcome::Failed,
                "abandoned work is not success"
            );
            self.started.notify_one();
            self.gate.notified().await;
            self.host.release(env, outcome).await
        }
    }

    struct Finished(oneshot::Sender<()>);

    #[async_trait::async_trait]
    impl RunGuard for Finished {
        async fn teardown(self: Box<Self>) {
            let _ = self.0.send(());
        }
    }

    struct PausedDrop {
        started: Arc<Notify>,
        gate:    sync_mpsc::Receiver<()>,
    }

    impl Drop for PausedDrop {
        fn drop(&mut self) {
            self.started.notify_one();
            // Hold destruction open so the test can cancel its join in flight.
            let _ = self.gate.recv_timeout(Duration::from_secs(10));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_finish_still_joins_the_worker_before_services() {
        let dir = RunDir::new("driver-finish-join-drop");
        let (finished, mut finish) = oneshot::channel();
        let mut driver = Driver::new(
            Graph::new(),
            Arc::new(HostExecutor::new(dir.path())),
            Registry::new(),
            Arc::new(MapSecrets::empty()),
            RunConfig::new(dir.path()),
        )
        .with_run_guard(Box::new(Finished(finished)));
        let dropping = Arc::new(Notify::new());
        let (release, gate) = sync_mpsc::channel();
        let paused = PausedDrop {
            started: dropping.clone(),
            gate,
        };
        let mut workers = JoinSet::new();
        let (started, start) = oneshot::channel();
        workers.spawn(async move {
            let _paused = paused;
            let _ = started.send(());
            pending::<()>().await;
        });
        start.await.expect("worker started");
        let (forwards, _forwarded) = mpsc::unbounded_channel();
        let firing = FiringId::new(1);
        let attempt = Attempt::new(1);
        driver.tasks.insert(firing, Task {
            name: "paused".into(),
            scope: ScopeId::new(0),
            attempt,
            started: time::Instant::now(),
            span: tracing::Span::none(),
            forwards,
            workers,
            deadline: None,
            reason: None,
            budget: None,
        });
        let mut finishing = Box::pin(driver.finish(
            firing,
            attempt,
            Outcome::new(Status::Cancelled, Value::Null),
            ResultOrigin::Driver,
        ));
        time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                () = dropping.notified() => {},
                () = &mut finishing => panic!("worker drop must finish first"),
            }
        })
        .await
        .expect("finish is joining the worker");
        drop(finishing);
        drop(driver);
        assert!(
            time::timeout(Duration::from_millis(50), &mut finish)
                .await
                .is_err(),
            "services must wait for a finish interrupted during worker shutdown"
        );
        release.send(()).expect("release the worker destructor");
        time::timeout(Duration::from_secs(10), finish)
            .await
            .expect("cleanup finished")
            .expect("guard teardown ran");
    }

    struct PausedTeardown {
        started: Arc<Notify>,
        gate:    Arc<Notify>,
        done:    oneshot::Sender<()>,
    }

    #[async_trait::async_trait]
    impl RunGuard for PausedTeardown {
        async fn teardown(self: Box<Self>) {
            self.started.notify_one();
            self.gate.notified().await;
            let _ = self.done.send(());
        }
    }

    #[tokio::test]
    async fn aborting_during_guard_teardown_completes_every_guard() {
        let dir = RunDir::new("driver-guard-teardown-drop");
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let (first, first_done) = oneshot::channel();
        let (last, last_done) = oneshot::channel();
        let driver = Driver::new(
            Graph::new(),
            Arc::new(HostExecutor::new(dir.path())),
            Registry::new(),
            Arc::new(MapSecrets::empty()),
            RunConfig::new(dir.path()),
        )
        .with_run_guard(Box::new(PausedTeardown {
            started: started.clone(),
            gate:    gate.clone(),
            done:    first,
        }))
        .with_run_guard(Box::new(Finished(last)));
        let run = tokio::spawn(driver.run());
        time::timeout(Duration::from_secs(10), started.notified())
            .await
            .expect("guard teardown started");
        run.abort();
        assert!(matches!(run.await, Err(error) if error.is_cancelled()));
        gate.notify_one();
        time::timeout(Duration::from_secs(10), async {
            first_done.await.expect("active guard teardown completed");
            last_done.await.expect("remaining guard teardown completed");
        })
        .await
        .expect("every guard completed");
    }

    #[tokio::test]
    async fn an_aborted_acquires_completed_result_is_released_before_services() {
        let dir = RunDir::new("driver-acquire-handoff-drop");
        let executor = Arc::new(GatedRelease {
            host:    HostExecutor::new(dir.path()).with_retention(Retention::OnFailure),
            started: Notify::new(),
            gate:    Notify::new(),
        });
        let env = executor
            .acquire(
                &ScopeSpec::new(ScopeId::new(0), "scope-0"),
                &AcquireContext::bare(),
            )
            .await
            .expect("acquire");
        let (finished, mut finish) = oneshot::channel();
        let mut driver = Driver::new(
            Graph::new(),
            executor.clone(),
            Registry::new(),
            Arc::new(MapSecrets::empty()),
            RunConfig::new(dir.path()),
        )
        .with_run_guard(Box::new(Finished(finished)));
        let acquired = AcquiredScope::new(env, driver.abandoned_tx.clone());
        // Reproduce an acquire that completed but has not handed its result
        // to the signal channel. Aborting drops the result after Driver::drop
        // has already closed that channel and scheduled its cleanup.
        let (started, start) = oneshot::channel();
        driver.background.spawn(async move {
            let _acquired = acquired;
            let _ = started.send(());
            pending::<()>().await;
        });
        start
            .await
            .expect("the acquired result belongs to the task");
        drop(driver);
        time::timeout(Duration::from_secs(1), executor.started.notified())
            .await
            .expect("the abandoned acquire's result is released");
        assert!(
            time::timeout(Duration::from_millis(50), &mut finish)
                .await
                .is_err(),
            "services must stay alive while the abandoned result is released"
        );
        executor.gate.notify_one();
        time::timeout(Duration::from_secs(10), finish)
            .await
            .expect("cleanup finished")
            .expect("guard teardown ran");
        assert!(
            dir.workspace().exists(),
            "failure retention preserves the workspace"
        );
    }
}
