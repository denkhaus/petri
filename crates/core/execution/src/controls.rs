//! Run controls a host drives while a run is live: pause and unpause at node
//! admission, steering into an active stage, and cancellation.
//!
//! One [`ControlService`] serves the terminal (`petri run --control <path>`)
//! and an embedded host alike. It is built from three pieces that already
//! exist: the coordinator's handle (cancellation and delivery), an admission
//! middleware ([`PauseGate`]) that holds every new attempt while the run is
//! paused, and an [`ExecutionObserver`] that keeps the live firing of each
//! node so a steer can name a stage instead of a firing.
//!
//! # Pause
//!
//! A paused run admits no new attempt: the gate's `admit` waits at the
//! awaited admission point, so a firing that was about to start keeps its
//! identity and starts no attempt until `resume`. Work already running keeps
//! running, and cancellation stays responsive: a cancel settles a firing that
//! is waiting on admission the way the engine always has.
//!
//! # Steering
//!
//! A steer is a [`Steer`] payload delivered to a live firing. It is never an
//! answer: a human gate ignores it and keeps its question open, and an agent
//! step queues it as guidance for its session. Control input therefore cannot
//! consume a pending question's answer.
//!
//! Task 10's awaited-admission extension point is the general form of the
//! gate below; when it lands the gate becomes one adapter over it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use engine::{Admission, EngineState, Event, EventRecord, MiddlewareKey};
use ir::FiringId;
use serde_json::Value;
use smol_str::SmolStr;
use steps::Steer;
use tokio::sync::watch;

use crate::{
    AdmitCall, AdmitNext, CoordinatorEvent, CoordinatorHandle, CoordinatorRecord, ExecutionId,
    ExecutionObserver, InvocationId, Middleware, MiddlewareError,
};

/// The chain key the pause gate records itself under.
pub const PAUSE_KEY: &str = "pause-gate";

/// Admission middleware that holds new attempts while paused. Pause state is
/// live, not durable: a resumed run starts unpaused.
pub struct PauseGate {
    paused: watch::Receiver<bool>,
}

#[async_trait::async_trait]
impl Middleware for PauseGate {
    fn key(&self) -> MiddlewareKey {
        MiddlewareKey::new(PAUSE_KEY)
    }

    fn state_version(&self) -> u32 {
        1
    }

    fn initial_state(&self) -> Value {
        Value::Null
    }

    fn fold(
        &self,
        _state: &mut Value,
        _event: &crate::FoldEvent<'_>,
    ) -> Result<(), MiddlewareError> {
        Ok(())
    }

    async fn admit(
        &self,
        call: AdmitCall,
        next: AdmitNext<'_>,
    ) -> Result<Admission, MiddlewareError> {
        let mut paused = self.paused.clone();
        if *paused.borrow() {
            tracing::info!(
                execution = call.address.execution.raw(),
                decision = ?call.address.decision,
                "admission held: the run is paused"
            );
            // A closed sender means the service is gone; admit rather than
            // hold the run hostage to a dropped controller.
            while *paused.borrow_and_update() {
                if paused.changed().await.is_err() {
                    break;
                }
            }
        }
        next.run().await
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

/// The one control service. Construct before the run, take its
/// [`gate`](Self::gate) into the coordinator's middleware chain, observe a
/// clone, [`wire`](Self::wire) the handle once the coordinator exists, then
/// drive it from wherever controls come from.
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

    /// The admission middleware that enforces pauses. Install it in the
    /// coordinator's chain; without it `pause` is recorded but holds nothing.
    pub fn gate(&self) -> Arc<dyn Middleware> {
        Arc::new(PauseGate {
            paused: self.inner.paused.subscribe(),
        })
    }

    pub fn wire(&self, handle: CoordinatorHandle) {
        *self
            .inner
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(handle);
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

    /// Hold every attempt not yet admitted. Running work is not interrupted.
    pub fn pause(&self) {
        if self.inner.paused.send_replace(true) {
            return;
        }
        tracing::info!("run paused: new attempts are held at admission");
    }

    /// Let held and future attempts through.
    pub fn unpause(&self) {
        if !self.inner.paused.send_replace(false) {
            return;
        }
        tracing::info!("run resumed");
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
        self.handle()?.cancel_root();
        Ok(())
    }
}

impl ExecutionObserver for ControlService {
    fn on_engine_record(&self, execution: ExecutionId, record: &EventRecord, state: &EngineState) {
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
        } = &record.event
        {
            self.inner.live().executions.insert(*execution, *invocation);
        }
    }
}
