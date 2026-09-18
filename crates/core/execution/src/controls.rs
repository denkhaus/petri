//! Run controls a host drives while a run is live: pause and unpause at node
//! admission, steering into an active stage, and cancellation.
//!
//! One [`ControlService`] serves the terminal (`petri run --control <path>`)
//! and an embedded host alike. It is built from pieces that already exist:
//! the coordinator's handle (cancellation and delivery), the driver's awaited
//! [`ExecutionHooks::before_attempt`] admission point (the pause), and an
//! [`ExecutionObserver`] that keeps the live firing of each node so a steer
//! can name a stage instead of a firing.
//!
//! # Pause
//!
//! A paused run admits no new attempt: the service's `before_attempt` waits
//! until the run is unpaused, so a firing that was about to start keeps its
//! one identity, starts no attempt, and does not count as a visit twice.
//! Work already running keeps running, and cancellation stays responsive: a
//! cancel settles a firing that is waiting on admission as `Cancelled`, the
//! way the engine always has.
//!
//! The pause is durable. Each pause and unpause is a coordinator record
//! (`RunPaused`, `RunUnpaused`), so `replay_run` carries `run_paused` and
//! `run_unpaused`, `petri inspect` reports `paused`, and a resume whose last
//! recorded control was a pause starts with admission held: the service is
//! handed the replayed state ([`ExecutionObserver::on_resumed`]) before the
//! first attempt is admitted, and nothing runs until an unpause arrives. The
//! two controls order themselves against the record differently. A pause
//! holds admission at once and records after: holding early is safe. An
//! unpause records first and releases after, so a crash right after an
//! unpause never resumes paused; [`ControlService::unpause`] is therefore
//! `async` and returns once the record is durable.
//!
//! # Steering
//!
//! A steer is a [`Steer`] payload delivered to a live firing. It is never an
//! answer: a human gate ignores it and keeps its question open, and an agent
//! step queues it as guidance for its session. Control input therefore cannot
//! consume a pending question's answer.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use driver::lifecycle::{
    AdmitAttempt, AttemptDecision, ExecutionHooks, HookContext, Note, PrepareError, PrepareResult,
    Prepared, Recorded, RunFinished, ScopeAcquired, ScopeAcquiredError, ScopeReleased, Transition,
    TransitionError, TransitionReport,
};
use engine::{EngineState, Event, EventRecord};
use ir::FiringId;
use smol_str::SmolStr;
use steps::Steer;
use tokio::sync::watch;

use crate::{
    CancelReason, CoordinatorEvent, CoordinatorHandle, CoordinatorRecord, CoordinatorState,
    ExecutionId, ExecutionObserver, InvocationId,
};

/// The service's [`ExecutionHooks`]: `before_attempt` holds while paused and
/// every other point delegates to the host's own hooks, when it has some.
/// Install it with [`Runtime::hooks`](runtime::Runtime::hooks).
pub struct PauseHooks {
    paused: watch::Receiver<bool>,
    inner:  Option<Arc<dyn ExecutionHooks>>,
}

#[async_trait::async_trait]
impl ExecutionHooks for PauseHooks {
    async fn before_attempt(
        &self,
        context: &HookContext,
        request: AdmitAttempt,
    ) -> AttemptDecision {
        let mut paused = self.paused.clone();
        if *paused.borrow() {
            tracing::info!(
                firing = request.view.firing.raw(),
                attempt = request.view.attempt.raw(),
                node = request.view.node_name(),
                "admission held: the run is paused"
            );
            // A closed sender means the service is gone; admit rather than
            // hold the run hostage to a dropped controller.
            while *paused.borrow_and_update() {
                if paused.changed().await.is_err() {
                    break;
                }
            }
            tracing::info!(
                firing = request.view.firing.raw(),
                node = request.view.node_name(),
                "admission released"
            );
        }
        match &self.inner {
            Some(inner) => inner.before_attempt(context, request).await,
            None => AttemptDecision::admit(),
        }
    }

    async fn prepare_result(
        &self,
        context: &HookContext,
        request: PrepareResult,
    ) -> Result<Prepared, PrepareError> {
        match &self.inner {
            Some(inner) => inner.prepare_result(context, request).await,
            None => Ok(Prepared::unchanged()),
        }
    }

    async fn after_record(&self, context: &HookContext, recorded: Recorded) -> Vec<Note> {
        match &self.inner {
            Some(inner) => inner.after_record(context, recorded).await,
            None => Vec::new(),
        }
    }

    async fn transition(
        &self,
        context: &HookContext,
        transition: Transition,
    ) -> Result<TransitionReport, TransitionError> {
        match &self.inner {
            Some(inner) => inner.transition(context, transition).await,
            None => Ok(TransitionReport::default()),
        }
    }

    async fn run_finished(&self, context: &HookContext, finished: RunFinished) -> Vec<Note> {
        match &self.inner {
            Some(inner) => inner.run_finished(context, finished).await,
            None => Vec::new(),
        }
    }

    async fn scope_released(&self, context: &HookContext, released: ScopeReleased) -> Vec<Note> {
        match &self.inner {
            Some(inner) => inner.scope_released(context, released).await,
            None => Vec::new(),
        }
    }

    async fn scope_acquired(
        &self,
        context: &HookContext,
        acquired: ScopeAcquired,
    ) -> Result<(), ScopeAcquiredError> {
        match &self.inner {
            Some(inner) => inner.scope_acquired(context, acquired).await,
            None => Ok(()),
        }
    }
}

/// Where a live firing runs, for a steer by node name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveStage {
    pub invocation: InvocationId,
    pub execution:  ExecutionId,
    pub firing:     FiringId,
}

#[derive(Default)]
struct Live {
    executions: BTreeMap<ExecutionId, InvocationId>,
    /// Live firings by node instance name; a name running in two executions
    /// keeps the latest.
    stages:     BTreeMap<SmolStr, LiveStage>,
    firings:    BTreeMap<(ExecutionId, FiringId), SmolStr>,
}

/// Why a control could not be applied.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ControlError {
    #[error("no stage named `{0}` is running")]
    NoSuchStage(String),
    #[error("the stage is no longer live")]
    NotLive,
    #[error("the run has finished")]
    Finished,
}

struct Inner {
    paused: watch::Sender<bool>,
    handle: Mutex<Option<CoordinatorHandle>>,
    live:   Mutex<Live>,
}

impl Inner {
    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The one control service. Construct before the run, install its
/// [`hooks`](Self::hooks) on the runtime, observe a clone, [`wire`](Self::wire)
/// the handle once the coordinator exists, then drive it from wherever
/// controls come from.
#[derive(Clone)]
pub struct ControlService {
    inner: Arc<Inner>,
}

impl Default for ControlService {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlService {
    pub fn new() -> Self {
        let (paused, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                paused,
                handle: Mutex::new(None),
                live: Mutex::new(Live::default()),
            }),
        }
    }

    /// The execution hooks that enforce pauses at `before_attempt`, over the
    /// host's own hooks when it has some. Without them installed, `pause` is
    /// recorded but holds nothing.
    pub fn hooks(&self, inner: Option<Arc<dyn ExecutionHooks>>) -> Arc<dyn ExecutionHooks> {
        Arc::new(PauseHooks {
            paused: self.inner.paused.subscribe(),
            inner,
        })
    }

    /// Hand the service the run's handle. A pause taken before this point is
    /// recorded now; a redundant record (the run resumed paused) is skipped
    /// by the coordinator.
    pub fn wire(&self, handle: CoordinatorHandle) {
        *self
            .inner
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(handle.clone());
        if self.is_paused() {
            tokio::spawn(async move { handle.set_paused(true).await });
        }
    }

    fn handle(&self) -> Result<CoordinatorHandle, ControlError> {
        self.inner
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(ControlError::Finished)
    }

    pub fn is_paused(&self) -> bool {
        *self.inner.paused.borrow()
    }

    /// Every change of the paused state, for a projector that publishes
    /// `run_paused` and `run_unpaused`.
    pub fn paused_changes(&self) -> watch::Receiver<bool> {
        self.inner.paused.subscribe()
    }

    /// Hold every attempt not yet admitted. Running work is not interrupted.
    /// The hold is immediate; the durable record follows through the
    /// coordinator when the service is wired (else when it is).
    pub fn pause(&self) {
        if self.inner.paused.send_replace(true) {
            return;
        }
        tracing::info!("run paused: new attempts are held at admission");
        if let Ok(handle) = self.handle() {
            tokio::spawn(async move { handle.set_paused(true).await });
        }
    }

    /// Let held and future attempts through. The unpause is recorded first
    /// and admission is released once the record is durable, so a crash in
    /// between resumes paused, never the other way round. Without a live
    /// coordinator the release is immediate.
    pub async fn unpause(&self) {
        if !*self.inner.paused.borrow() {
            return;
        }
        if let Ok(handle) = self.handle() {
            handle.set_paused(false).await;
        }
        if self.inner.paused.send_replace(false) {
            tracing::info!("run resumed");
        }
    }

    /// The live firing of a node instance, by name.
    pub fn stage(&self, node: &str) -> Option<LiveStage> {
        self.inner.live().stages.get(node).cloned()
    }

    /// Every live stage, by node name.
    pub fn stages(&self) -> BTreeMap<SmolStr, LiveStage> {
        self.inner.live().stages.clone()
    }

    /// Deliver guidance to the named stage's live firing.
    pub async fn steer(&self, node: &str, text: impl Into<String>) -> Result<(), ControlError> {
        let stage = self
            .stage(node)
            .ok_or_else(|| ControlError::NoSuchStage(node.to_owned()))?;
        self.steer_firing(stage.execution, stage.firing, text).await
    }

    /// Deliver guidance to one firing.
    pub async fn steer_firing(
        &self,
        execution: ExecutionId,
        firing: FiringId,
        text: impl Into<String>,
    ) -> Result<(), ControlError> {
        let handle = self.handle()?;
        match handle
            .deliver(execution, firing, Steer::new(text).to_control())
            .await
        {
            driver::DeliverDisposition::Delivered => Ok(()),
            driver::DeliverDisposition::NotLive => Err(ControlError::NotLive),
        }
    }

    /// Cancel the whole run politely; a second call reaches the kill tier.
    pub fn cancel(&self) -> Result<(), ControlError> {
        self.handle()?.cancel_root_for(CancelReason::Control);
        Ok(())
    }
}

impl ExecutionObserver for ControlService {
    fn on_engine_record(
        &self,
        execution: ExecutionId,
        record: &EventRecord,
        _recorded_at: u64,
        state: &EngineState,
    ) {
        match &record.event {
            Event::StepStarted { firing, .. } => {
                let Some(name) = state
                    .firing_node(*firing)
                    .and_then(|id| state.graph().node(id))
                    .map(|node| node.name.clone())
                else {
                    return;
                };
                let mut live = self.inner.live();
                let invocation = live
                    .executions
                    .get(&execution)
                    .copied()
                    .unwrap_or(InvocationId::ROOT);
                live.stages.insert(name.clone(), LiveStage {
                    invocation,
                    execution,
                    firing: *firing,
                });
                live.firings.insert((execution, *firing), name);
            }
            Event::StepFinished { firing, .. } => {
                let mut live = self.inner.live();
                if let Some(name) = live.firings.remove(&(execution, *firing))
                    && live.stages.get(&name).is_some_and(|stage| {
                        stage.execution == execution && stage.firing == *firing
                    })
                {
                    live.stages.remove(&name);
                }
            }
            _ => {}
        }
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        if let CoordinatorEvent::ExecutionDeclared {
            execution,
            invocation,
            ..
        } = &record.body
        {
            self.inner.live().executions.insert(*execution, *invocation);
        }
    }

    /// Start where the log left the run: held at admission when the last
    /// recorded control was a pause, and knowing every declared execution.
    fn on_resumed(&self, state: &CoordinatorState) {
        {
            let mut live = self.inner.live();
            for (execution, declared) in &state.executions {
                live.executions
                    .insert(*execution, declared.declaration.invocation);
            }
        }
        if state.paused && !self.inner.paused.send_replace(true) {
            tracing::info!("resumed paused: attempts are held until an unpause");
        }
    }
}
