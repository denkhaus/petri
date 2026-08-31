//! The driver loop.

use std::any::Any;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engine::{
    CANCEL_ESCALATION_KEY, Command, EngineState, Event, EventLog, ReplayMismatch, ResolvedFiring,
    apply,
};
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, NoProgress, ProgressSink, ReleaseReport,
    Retention, ScopeOutcome, ScopeSpec, SecretProvider,
};
use ir::placeholder::SECRET_REF_KEY;
use ir::{
    Attempt, Control, EvalEnv, ExprOrValue, FailureClass, FailureInfo, FiringId, Graph, NodeId,
    Outcome, RunContext, RunStatus, ScopeId, StaticCtx, Status, StepEvent, Value, eval,
};
use smol_str::SmolStr;
use steps::{Capabilities, Registry, StepCtx};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time;
use tracing::Instrument as _;

use crate::jitter::jittered;
use crate::observe::{EventObserver, ObserveError};
use crate::sink::LogSink;

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
    pub fn echoing(mut self, echo: bool) -> Self {
        self.echo_logs = echo;
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
pub struct RunReport {
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
    Timeout {
        firing:  FiringId,
        attempt: Attempt,
    },
    RetryDue {
        firing:       FiringId,
        next_attempt: Attempt,
    },
    HardDeadline {
        firing:  FiringId,
        attempt: Attempt,
    },
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
    join:     JoinHandle<()>,
    timeout:  Option<JoinHandle<()>>,
    deadline: Option<JoinHandle<()>>,
    reason:   Option<CancelReason>,
}

pub struct Driver {
    engine:           EngineState,
    executor:         Arc<dyn Executor>,
    runners:          Arc<Registry>,
    secrets:          Arc<dyn SecretProvider>,
    progress:         Arc<dyn ProgressSink>,
    sink:             Arc<LogSink>,
    config:           RunConfig,
    envs:             HashMap<ScopeId, EnvHandle>,
    acquire_failures: HashMap<ScopeId, String>,
    scope_failed:     HashSet<ScopeId>,
    tasks:            HashMap<FiringId, Task>,
    caps:             Capabilities,
    observers:        Vec<Arc<dyn EventObserver>>,
    /// Per-run host services riding this run's lifetime: held untouched until
    /// the driver drops, which is their teardown.
    run_guards:       Vec<Box<dyn Any + Send + Sync>>,
    releases:         Vec<JoinHandle<ReleaseReport>>,
    /// Armed by the first root cancel; expiry feeds back `KillRequested`.
    cleanup_timer:    Option<JoinHandle<()>>,
    /// Set by [`Driver::resume`]; consumed at the top of [`Driver::run`].
    resume:           Option<PendingResume>,
    tx:               mpsc::Sender<Signal>,
    rx:               mpsc::Receiver<Signal>,
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
        let sink =
            Arc::new(LogSink::new(&config.run_dir, secrets.masker()).echoing(config.echo_logs));
        let (tx, rx) = mpsc::channel(1024);
        Self {
            engine,
            executor,
            runners: Arc::new(runners),
            secrets,
            progress: Arc::new(NoProgress),
            sink,
            config,
            envs: HashMap::new(),
            acquire_failures: HashMap::new(),
            scope_failed: HashSet::new(),
            tasks: HashMap::new(),
            caps: Capabilities::default(),
            observers: Vec::new(),
            run_guards: Vec::new(),
            releases: Vec::new(),
            cleanup_timer: None,
            resume: None,
            tx,
            rx,
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

    /// Hold a per-run host service for this run's lifetime. The driver never
    /// looks inside; dropping the driver drops the guard, which is the
    /// service's teardown.
    #[must_use]
    pub fn with_run_guard(mut self, guard: Box<dyn Any + Send + Sync>) -> Self {
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

    /// Run to completion.
    #[tracing::instrument(
        name = "driver.run",
        skip_all,
        fields(
            run_dir = %self.config.run_dir.display(),
            resumed = self.resume.is_some(),
            node_count = self.engine.graph.nodes.len(),
        )
    )]
    pub async fn run(mut self) -> RunReport {
        match self.resume.take() {
            None => self.feed(Event::RunStarted).await,
            Some(resume) => {
                // Observers see the regenerated suffix first — the records past
                // the loaded prefix, which the crash kept off disk — before any
                // pending command is dispatched, so a store attaching here
                // starts from a converged view.
                self.notify_observers(resume.suffix_from);
                for command in resume.commands {
                    self.dispatch(command).await;
                }
            }
        }

        while !self.engine.is_finished() {
            let Some(signal) = self.rx.recv().await else {
                break;
            };
            self.on_signal(signal).await;
        }
        if let Some(timer) = self.cleanup_timer.take() {
            timer.abort();
        }

        // Release is best effort and never fails the run, but the run should not
        // report back before the environments are actually gone.
        let mut releases = Vec::new();
        for handle in mem::take(&mut self.releases) {
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

        RunReport {
            status,
            state: self.engine,
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
            Signal::Inject(Event::CancelRequested { scope })
                if scope == ir::CancelScopeId::ROOT =>
            {
                self.on_root_cancel().await;
            }
            Signal::Inject(Event::KillRequested { scope }) if scope == ir::CancelScopeId::ROOT => {
                self.kill_root().await;
            }
            Signal::Inject(event) => self.feed(event).await,
            Signal::Deliver { firing, ctl, ack } => self.on_deliver(firing, ctl, ack),
            Signal::Progress { firing, event } => {
                let event = self.mask_progress(firing, event).await;
                self.feed(Event::StepProgress { firing, ev: event }).await;
            }
            Signal::Finished {
                firing,
                attempt,
                outcome,
            } => self.finish(firing, attempt, outcome).await,
            Signal::Timeout { firing, attempt } => self.on_timeout(firing, attempt),
            Signal::RetryDue {
                firing,
                next_attempt,
            } => {
                self.feed(Event::RetryElapsed {
                    firing,
                    next_attempt,
                })
                .await;
            }
            Signal::HardDeadline { firing, attempt } => {
                self.on_hard_deadline(firing, attempt).await;
            }
        }
    }

    /// The two-tier stop wiring (§10). The first root cancel feeds
    /// `CancelRequested` and arms the cleanup-grace timer; expiry, or another
    /// root cancel (a CLI maps a second Ctrl-C to it), feeds `KillRequested`.
    /// Both are ordinary External events, so the hard stop is in the log and
    /// replay reproduces it. The engine's own state says which tier this is —
    /// `is_cancelled` is set by exactly the root cancel and the root kill — so
    /// the driver keeps no count of its own.
    async fn on_root_cancel(&mut self) {
        if self.engine.is_cancelled() {
            self.kill_root().await;
            return;
        }
        self.feed(Event::CancelRequested {
            scope: ir::CancelScopeId::ROOT,
        })
        .await;
        let grace = self.config.cleanup_grace;
        tracing::info!(
            live_firing_count = self.tasks.len(),
            cleanup_grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
            "run cancel requested"
        );
        let tx = self.tx.clone();
        self.cleanup_timer = Some(tokio::spawn(async move {
            time::sleep(grace).await;
            let _ = tx
                .send(Signal::Inject(Event::KillRequested {
                    scope: ir::CancelScopeId::ROOT,
                }))
                .await;
        }));
    }

    async fn kill_root(&mut self) {
        if let Some(timer) = self.cleanup_timer.take() {
            timer.abort();
        }
        tracing::warn!(live_firing_count = self.tasks.len(), "run kill requested");
        self.feed(Event::KillRequested {
            scope: ir::CancelScopeId::ROOT,
        })
        .await;
    }

    /// Append-then-apply, then dispatch whatever the core asked for.
    ///
    /// The append happens inside `apply`, which records this event as
    /// `External` and everything it derives as `Core`.
    async fn feed(&mut self, event: Event) {
        for command in self.apply_event(event) {
            self.dispatch(command).await;
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

    async fn dispatch(&mut self, command: Command) {
        match command {
            Command::AcquireScope { scope } => self.acquire(scope).await,
            Command::ReleaseScope { scope } => self.release(scope),
            Command::StartStep(resolved) => {
                let (firing, attempt) = (resolved.id(), resolved.attempt());
                let live = self.engine.firing(firing);
                // A firing the stop tiers marked cancelling reaches dispatch
                // only on resume, and is never re-spawned: it finishes directly
                // with the same `Cancelled` outcome the live run would have fed
                // once the stop landed, tagged with the tier from the replayed
                // state (§10).
                if let Some(node) = live
                    .filter(|state| state.cancelling)
                    .map(|state| state.node)
                {
                    self.finish_instead_of_resuming(firing, attempt, node);
                    return;
                }
                // A started firing is already acknowledged in the loaded log; a
                // second `StepStarted` would be a replay divergence.
                let started = live.is_some_and(|state| state.started);
                if !started {
                    // The acknowledgement that the attempt was dispatched. It is
                    // applied here, before `start` can spawn the runner, so the
                    // log places it ahead of anything the runner sends back: an
                    // instant runner's `Finished` riding the signal channel can
                    // otherwise land first, and the late `StepStarted` then
                    // reports the finalized firing as unknown.
                    let commands = self.apply_event(Event::StepStarted { firing, attempt });
                    debug_assert!(commands.is_empty(), "StepStarted derives no commands");
                }
                self.start(&resolved);
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
                tokio::spawn(async move {
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
            Command::ExpandNode { .. } | Command::FinishRun { .. } => {}
        }
    }

    /// Finish a cancelling firing that resume would otherwise re-spawn: one
    /// code path for both tiers, the tier read from the replayed state. The
    /// finish rides the signal channel like every step result, so its place in
    /// the log is its arrival order.
    fn finish_instead_of_resuming(&self, firing: FiringId, attempt: Attempt, node: NodeId) {
        let escalation = if self.engine.is_node_killed(node) {
            &KILLED_BEFORE_RESUME
        } else {
            &CANCELLED_BEFORE_RESUME
        };
        let outcome = escalation_outcome(Status::Cancelled, escalation);
        let tx = self.tx.clone();
        tokio::spawn(async move {
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

    async fn acquire(&mut self, scope: ScopeId) {
        if self.envs.contains_key(&scope) {
            return;
        }
        let spec = self.scope_spec(scope);
        let ctx = AcquireContext::new(self.secrets.clone(), self.progress.clone());
        match self.executor.acquire(&spec, &ctx).await {
            Ok(handle) => {
                self.envs.insert(scope, handle);
            }
            Err(error) => {
                // Not a run abort: every firing in this scope fails, routably.
                // This is the deliberate render point — the failure becomes
                // `FailureInfo.message`, a rendered projection — so the whole
                // source chain is flattened into it here.
                self.acquire_failures.insert(scope, render_chain(&error));
            }
        }
    }

    fn release(&mut self, scope: ScopeId) {
        let Some(handle) = self.envs.remove(&scope) else {
            return;
        };
        let outcome = if self.scope_failed.contains(&scope) {
            ScopeOutcome::Failed
        } else {
            ScopeOutcome::Succeeded
        };
        let executor = self.executor.clone();
        self.releases.push(tokio::spawn(async move {
            executor.release(handle, outcome).await
        }));
    }

    fn scope_spec(&self, scope: ScopeId) -> ScopeSpec {
        let mut spec =
            ScopeSpec::new(scope, &format!("scope-{}", scope.raw())).with_grace(self.config.grace);
        let Some(definition) = self.engine.graph.scope(scope) else {
            return spec;
        };
        // Scope env is resolved once, against the run parameters and nothing else:
        // it cannot depend on a firing.
        let empty_run = RunContext::new();
        let mut params_only = StaticCtx::new();
        for (key, value) in &self.engine.graph.params {
            params_only.set(key, value.clone());
        }
        let env_context = EvalEnv::new(&Value::Null, &empty_run, &params_only);
        let resolve = |entries: &BTreeMap<SmolStr, ExprOrValue>| -> BTreeMap<SmolStr, SmolStr> {
            let mut env: BTreeMap<SmolStr, SmolStr> = BTreeMap::new();
            for (key, value) in entries {
                let resolved = match value {
                    ExprOrValue::Value(v) => value_to_string(v),
                    ExprOrValue::Expr(id) => {
                        match eval(&self.engine.graph.exprs, *id, &env_context) {
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
                ports:       service.ports.clone(),
                options:     service.options.clone(),
                credentials: service.credentials.clone(),
            })
            .collect();
        spec = spec
            .with_env(resolve(&definition.env))
            .with_runtime(definition.runtime.clone())
            .with_services(services);
        spec
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
            tokio::spawn(async move {
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
        let Some((name, kind, runner)) = self.engine.graph.node(node).and_then(|n| {
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
        let (forward_tx, mut forward_rx) = mpsc::unbounded_channel::<Forward>();
        tokio::spawn(async move {
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
        tokio::spawn(async move {
            while let Some(event) = log_rx.recv().await {
                if progress_tx
                    .send(Signal::Progress { firing, event })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });

        let ctx = StepCtx {
            firing,
            attempt,
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
        let join = tokio::spawn(
            async move {
                let outcome = runner.run(ctx).await;
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

        // The per-attempt timeout. `Budget.timeout` is per attempt, not per firing.
        let mut timeout = None;
        if let Some(limit) = self.engine.graph.node(node).map(|n| n.budget.timeout)
            && !limit.is_zero()
        {
            let tx = self.tx.clone();
            timeout = Some(tokio::spawn(async move {
                time::sleep(limit).await;
                let _ = tx.send(Signal::Timeout { firing, attempt }).await;
            }));
        }

        self.tasks.insert(firing, Task {
            name,
            scope,
            attempt,
            span,
            forwards: forward_tx,
            join,
            timeout,
            deadline: None,
            reason: None,
        });
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
        tokio::spawn(async move {
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
            task.deadline = Some(tokio::spawn(async move {
                time::sleep(limit).await;
                let _ = tx.send(Signal::HardDeadline { firing, attempt }).await;
            }));
        }
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
                }
                if let Some(ack) = ack {
                    let _ = ack.send(DeliverDisposition::NotLive);
                }
                return;
            }
        };
        let Some(task) = self.tasks.get(&firing) else {
            if let Some(ack) = ack {
                let _ = ack.send(DeliverDisposition::NotLive);
            }
            return;
        };
        if let Err(rejected) = task.forwards.send(Forward {
            ctl: Control::Deliver(payload),
            ack,
        }) && let Some(ack) = rejected.0.ack
        {
            let _ = ack.send(DeliverDisposition::NotLive);
        }
    }

    fn on_timeout(&mut self, firing: FiringId, attempt: Attempt) {
        let still_running = self
            .tasks
            .get(&firing)
            .is_some_and(|task| task.attempt == attempt);
        if !still_running {
            return;
        }
        self.stop_step(firing, Control::Cancel, CancelReason::TimedOut);
    }

    async fn on_hard_deadline(&mut self, firing: FiringId, attempt: Attempt) {
        let Some(task) = self.tasks.get(&firing) else {
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
        task.join.abort();

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
        self.finish(firing, attempt, escalation_outcome(status, &CANCEL_FORCED))
            .await;
    }

    async fn finish(&mut self, firing: FiringId, attempt: Attempt, outcome: Outcome) {
        // A firing finished from `fail_now` or from resume's direct finish has no
        // task, and so no step span: those events belong to the run instead.
        let (reason, span) = match self.tasks.remove(&firing) {
            Some(task) => {
                if let Some(timer) = task.timeout {
                    timer.abort();
                }
                if let Some(deadline) = task.deadline {
                    deadline.abort();
                }
                task.join.abort();
                if outcome.status.is_failure() {
                    self.scope_failed.insert(task.scope);
                }
                (task.reason, task.span)
            }
            None => (None, tracing::Span::current()),
        };

        // A step reports `Cancelled` whichever way it was stopped; only the driver
        // knows a timer got there first.
        let mut outcome = outcome;
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

        self.feed(Event::StepFinished {
            firing,
            attempt,
            outcome,
        })
        .await;
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
