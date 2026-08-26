//! The driver loop.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engine::{Command, EngineState, Event, ResolvedFiring, apply};
use executor::{
    EnvError, EnvHandle, Executor, ReleaseReport, Retention, ScopeOutcome, ScopeSpec,
    SecretProvider,
};
use ir::{
    Attempt, Control, EvalEnv, ExprOrValue, FailureInfo, FiringId, Graph, Outcome, RunContext,
    RunStatus, ScopeId, StaticCtx, Status, StepEvent, Value, eval,
};
use smol_str::SmolStr;
use steps::{Registry, StepCtx};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::jitter::jittered;
use crate::sink::LogSink;

/// The failure class recorded when the driver had to abort a step that ignored
/// `Control::Cancel`.
pub const CANCEL_FORCED: &str = "cancel_forced";

/// No step kind is registered for a node's `StepRef.kind`.
///
/// `validate_with(graph, Some(&registry))` reports this at load; the guard here is
/// the backstop for a caller that skipped validation.
pub const NO_RUNNER: &str = "no_runner";

/// After the first root cancel, how long admitted cleanup gets before the driver
/// feeds back `KillRequested` (§10, resolved decision 3).
pub const DEFAULT_CLEANUP_GRACE: Duration = Duration::from_secs(120);

/// Knobs, with the defaults from the handoff's table.
#[derive(Clone, Debug)]
pub struct RunConfig {
    pub run_dir: PathBuf,
    /// Between `SIGTERM` and `SIGKILL`, per scope.
    pub grace: Duration,
    /// How much longer than `grace` a step gets before the driver stops waiting.
    pub hard_deadline_slack: Duration,
    /// Between the first root cancel and the `KillRequested` that ends whatever
    /// cleanup is still running.
    pub cleanup_grace: Duration,
    pub keep_workspaces: Retention,
    /// Echo step output to this process's stdout.
    pub echo_logs: bool,
}

impl RunConfig {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir: run_dir.into(),
            grace: executor::DEFAULT_GRACE,
            hard_deadline_slack: Duration::from_secs(5),
            cleanup_grace: DEFAULT_CLEANUP_GRACE,
            keep_workspaces: Retention::default(),
            echo_logs: false,
        }
    }

    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    pub fn with_cleanup_grace(mut self, cleanup_grace: Duration) -> Self {
        self.cleanup_grace = cleanup_grace;
        self
    }

    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.keep_workspaces = retention;
        self
    }

    pub fn echoing(mut self, echo: bool) -> Self {
        self.echo_logs = echo;
        self
    }
}

/// How a run ended, and what it left behind.
pub struct RunReport {
    pub status: RunStatus,
    pub state: EngineState,
    pub releases: Vec<ReleaseReport>,
}

/// Why a step was told to stop. The distinction cannot be made by the step — only
/// the driver knows which arrived first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelReason {
    Requested,
    TimedOut,
}

/// Cancel a running run from outside it.
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
}

/// Everything that reaches the loop, through one channel, in arrival order.
enum Signal {
    /// An event from outside the run entirely, such as an operator cancelling.
    Inject(Event),
    Progress {
        firing: FiringId,
        event: StepEvent,
    },
    Finished {
        firing: FiringId,
        attempt: Attempt,
        outcome: Outcome,
    },
    Timeout {
        firing: FiringId,
        attempt: Attempt,
    },
    RetryDue {
        firing: FiringId,
        next_attempt: Attempt,
    },
    HardDeadline {
        firing: FiringId,
        attempt: Attempt,
    },
}

struct Task {
    name: SmolStr,
    scope: ScopeId,
    attempt: Attempt,
    control: mpsc::Sender<Control>,
    join: JoinHandle<()>,
    timeout: Option<JoinHandle<()>>,
    deadline: Option<JoinHandle<()>>,
    reason: Option<CancelReason>,
}

pub struct Driver {
    engine: EngineState,
    executor: Arc<dyn Executor>,
    runners: Arc<Registry>,
    secrets: Arc<dyn SecretProvider>,
    sink: Arc<LogSink>,
    config: RunConfig,
    envs: HashMap<ScopeId, EnvHandle>,
    acquire_failures: HashMap<ScopeId, String>,
    scope_failed: HashSet<ScopeId>,
    tasks: HashMap<FiringId, Task>,
    releases: Vec<JoinHandle<ReleaseReport>>,
    /// How many root cancels have arrived. The first is polite; the second is a
    /// kill. The driver never decides to stop the run by itself beyond this
    /// count: it feeds events and the core decides.
    root_cancels: u32,
    /// Armed by the first root cancel; expiry feeds back `KillRequested`.
    cleanup_timer: Option<JoinHandle<()>>,
    tx: mpsc::Sender<Signal>,
    rx: mpsc::Receiver<Signal>,
}

impl Driver {
    pub fn new(
        graph: Graph,
        executor: Arc<dyn Executor>,
        runners: Registry,
        secrets: Arc<dyn SecretProvider>,
        config: RunConfig,
    ) -> Self {
        let sink =
            Arc::new(LogSink::new(&config.run_dir, secrets.masker()).echoing(config.echo_logs));
        let (tx, rx) = mpsc::channel(1024);
        Self {
            engine: EngineState::new(graph),
            executor,
            runners: Arc::new(runners),
            secrets,
            sink,
            config,
            envs: HashMap::new(),
            acquire_failures: HashMap::new(),
            scope_failed: HashSet::new(),
            tasks: HashMap::new(),
            releases: Vec::new(),
            root_cancels: 0,
            cleanup_timer: None,
            tx,
            rx,
        }
    }

    /// Run to completion.
    pub async fn run(mut self) -> RunReport {
        self.feed(Event::RunStarted).await;

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
        for handle in std::mem::take(&mut self.releases) {
            if let Ok(report) = handle.await {
                releases.push(report);
            }
        }

        RunReport {
            status: self.engine.folded_status(),
            state: self.engine,
            releases,
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
                self.on_root_cancel().await
            }
            Signal::Inject(Event::KillRequested { scope })
                if scope == ir::CancelScopeId::ROOT =>
            {
                self.kill_root().await
            }
            Signal::Inject(event) => self.feed(event).await,
            Signal::Progress { firing, event } => {
                let event = self.mask_progress(firing, event).await;
                self.feed(Event::StepProgress { firing, ev: event }).await;
            }
            Signal::Finished {
                firing,
                attempt,
                outcome,
            } => self.finish(firing, attempt, outcome).await,
            Signal::Timeout { firing, attempt } => self.on_timeout(firing, attempt).await,
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
                self.on_hard_deadline(firing, attempt).await
            }
        }
    }

    /// The two-tier stop wiring (§10). The first root cancel feeds
    /// `CancelRequested` and arms the cleanup-grace timer; expiry, or another
    /// root cancel (a CLI maps a second Ctrl-C to it), feeds `KillRequested`.
    /// Both are ordinary External events, so the hard stop is in the log and
    /// replay reproduces it.
    async fn on_root_cancel(&mut self) {
        self.root_cancels += 1;
        if self.root_cancels > 1 {
            self.kill_root().await;
            return;
        }
        self.feed(Event::CancelRequested {
            scope: ir::CancelScopeId::ROOT,
        })
        .await;
        let grace = self.config.cleanup_grace;
        let tx = self.tx.clone();
        self.cleanup_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(grace).await;
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
        self.feed(Event::KillRequested {
            scope: ir::CancelScopeId::ROOT,
        })
        .await;
    }

    /// Append-then-apply, then dispatch whatever the core asked for.
    ///
    /// The append happens inside `apply`, which records this event as `External` and
    /// everything it derives as `Core`.
    async fn feed(&mut self, event: Event) {
        let state = std::mem::replace(&mut self.engine, EngineState::new(Graph::new()));
        let (state, commands) = apply(state, event);
        self.engine = state;
        for command in commands {
            self.dispatch(command).await;
        }
    }

    async fn dispatch(&mut self, command: Command) {
        match command {
            Command::AcquireScope { scope } => self.acquire(scope).await,
            Command::ReleaseScope { scope } => self.release(scope),
            Command::StartStep(resolved) => {
                let (firing, attempt) = (resolved.id(), resolved.attempt());
                self.start(resolved).await;
                // The acknowledgement that the attempt was dispatched. It goes through
                // the one channel like every other external event, so its place in the
                // log is its arrival order and replay feeds it back verbatim.
                let _ = self
                    .tx
                    .send(Signal::Inject(Event::StepStarted { firing, attempt }))
                    .await;
            }
            Command::DeliverControl { firing, ctl } => {
                self.deliver(firing, ctl, CancelReason::Requested).await
            }
            Command::ScheduleRetry {
                firing,
                next_attempt,
                base_delay,
            } => {
                let delay = jittered(base_delay, firing, next_attempt);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
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

    // ── Scopes ─────────────────────────────────────────────────────────────

    async fn acquire(&mut self, scope: ScopeId) {
        if self.envs.contains_key(&scope) {
            return;
        }
        let spec = self.scope_spec(scope);
        match self.executor.acquire(&spec).await {
            Ok(handle) => {
                self.envs.insert(scope, handle);
            }
            Err(error) => {
                // Not a run abort: every firing in this scope fails, routably.
                self.acquire_failures.insert(scope, error.to_string());
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
        let executor = Arc::clone(&self.executor);
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
        let mut env: BTreeMap<SmolStr, SmolStr> = BTreeMap::new();
        for (key, value) in &definition.env {
            let resolved = match value {
                ExprOrValue::Value(v) => value_to_string(v),
                ExprOrValue::Expr(id) => match eval(&self.engine.graph.exprs, *id, &env_context) {
                    Ok(v) => value_to_string(&v),
                    Err(_) => continue,
                },
            };
            env.insert(key.clone(), SmolStr::new(resolved));
        }
        spec = spec.with_env(env).with_runtime(definition.runtime.clone());
        spec
    }

    // ── Steps ──────────────────────────────────────────────────────────────

    async fn start(&mut self, resolved: ResolvedFiring) {
        let firing = resolved.id();
        let attempt = resolved.attempt();
        let scope = resolved.scope();
        let node = resolved.node();
        let name = self
            .engine
            .graph
            .node(node)
            .map(|n| n.name.clone())
            .unwrap_or_default();

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

        let Some(env) = self.envs.get(&scope).map(|handle| handle.exec()) else {
            self.fail_now(
                firing,
                attempt,
                "no environment was acquired for this scope",
                EnvError::ACQUIRE_CLASS,
            );
            return;
        };
        let Some(runner) = self
            .engine
            .graph
            .node(node)
            .and_then(|n| self.runners.get(&n.step.kind))
        else {
            self.fail_now(firing, attempt, "no runner for this step kind", NO_RUNNER);
            return;
        };

        let (log_tx, mut log_rx) = mpsc::channel::<StepEvent>(256);
        let (control_tx, control_rx) = mpsc::channel::<Control>(4);

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
            secrets: Arc::clone(&self.secrets),
            logs: log_tx,
            control: control_rx,
        };
        let done_tx = self.tx.clone();
        let join = tokio::spawn(async move {
            let outcome = runner.run(ctx).await;
            let _ = done_tx
                .send(Signal::Finished {
                    firing,
                    attempt,
                    outcome,
                })
                .await;
        });

        // The per-attempt timeout. `Budget.timeout` is per attempt, not per firing.
        let timeout = self
            .engine
            .graph
            .node(node)
            .map(|n| n.budget.timeout)
            .filter(|t| !t.is_zero())
            .map(|limit| {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(limit).await;
                    let _ = tx.send(Signal::Timeout { firing, attempt }).await;
                })
            });

        self.tasks.insert(
            firing,
            Task {
                name,
                scope,
                attempt,
                control: control_tx,
                join,
                timeout,
                deadline: None,
                reason: None,
            },
        );
    }

    fn fail_now(&mut self, firing: FiringId, attempt: Attempt, message: &str, class: &str) {
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
    async fn deliver(&mut self, firing: FiringId, ctl: Control, reason: CancelReason) {
        let Some(task) = self.tasks.get_mut(&firing) else {
            return;
        };
        // First reason wins: a timeout that beat a cancel makes this `TimedOut`.
        if task.reason.is_none() {
            task.reason = Some(reason);
        }
        let kill = matches!(ctl, Control::Kill);
        let _ = task.control.send(ctl).await;

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
                tokio::time::sleep(limit).await;
                let _ = tx.send(Signal::HardDeadline { firing, attempt }).await;
            }));
        }
    }

    async fn on_timeout(&mut self, firing: FiringId, attempt: Attempt) {
        let still_running = self
            .tasks
            .get(&firing)
            .is_some_and(|task| task.attempt == attempt);
        if !still_running {
            return;
        }
        self.deliver(firing, Control::Cancel, CancelReason::TimedOut)
            .await;
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
        // The step ignored its cancel. Stop waiting for it.
        task.join.abort();

        let mut output = serde_json::Map::new();
        output.insert(
            "cancel_escalation".into(),
            Value::String(CANCEL_FORCED.to_string()),
        );
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
        self.finish(firing, attempt, Outcome::new(status, Value::Object(output)))
            .await;
    }

    async fn finish(&mut self, firing: FiringId, attempt: Attempt, outcome: Outcome) {
        let reason = match self.tasks.remove(&firing) {
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
                task.reason
            }
            None => None,
        };

        // A step reports `Cancelled` whichever way it was stopped; only the driver
        // knows a timer got there first.
        let mut outcome = outcome;
        if reason == Some(CancelReason::TimedOut) && matches!(outcome.status, Status::Cancelled) {
            outcome.status = Status::TimedOut;
        }

        // Masking happens before the append, so the persisted log is post-mask.
        outcome.output = self.sink.mask_value(&outcome.output);
        outcome.context_updates = outcome
            .context_updates
            .iter()
            .map(|(k, v)| (k.clone(), self.sink.mask_value(v)))
            .collect();

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
            .map(|t| t.name.to_string())
            .unwrap_or_else(|| "step".to_string());
        match event {
            StepEvent::Log { stream, line } => {
                let line = self.sink.record(&name, firing.raw(), stream, &line).await;
                StepEvent::Log { stream, line }
            }
            StepEvent::Artifact { name, uri } => StepEvent::Artifact { name, uri },
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
