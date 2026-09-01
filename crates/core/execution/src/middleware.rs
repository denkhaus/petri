use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use driver::{
    AdmissionResolution, AdmitRequest, DecisionError, DecisionResolver, RoutingRequest,
    RoutingResolution, default_group_decision,
};
use engine::{
    Admission, DecisionId, Event, EventRecord, GroupDecision, Intervention, MiddlewareKey,
    RouteDecision, RoutingProposal,
};
use ir::{Attempt, FiringId, NodeId, Outcome, Value};
use smol_str::SmolStr;

use crate::{ExecutionId, InvocationId};

pub type MiddlewareState = BTreeMap<MiddlewareKey, (u32, Value)>;

#[derive(Clone, Debug)]
pub enum FoldEvent<'a> {
    ExecutionStarted,
    FinalOutcome {
        firing:  FiringId,
        node:    NodeId,
        outcome: &'a Outcome,
    },
    RouteApplied {
        firing:   FiringId,
        decision: RouteDecision,
    },
}

/// The middleware fold projection of one engine event.
///
/// The one definition both fold paths share: the live observer derives from the
/// post-apply state, and the resume rebuild derives from a replayed prefix —
/// only their finality and node lookups differ. Checkpointed middleware state
/// is validated across resume, so the two paths must fold identically.
pub(crate) fn derive_fold_event(
    event: &Event,
    is_final_attempt: impl Fn(FiringId, Attempt) -> bool,
    node_of: impl Fn(FiringId) -> Option<NodeId>,
) -> Option<FoldEvent<'_>> {
    match event {
        Event::ExecutionStarted(_) => Some(FoldEvent::ExecutionStarted),
        Event::StepFinished {
            firing,
            attempt,
            outcome,
        } if is_final_attempt(*firing, *attempt) => {
            node_of(*firing).map(|node| FoldEvent::FinalOutcome {
                firing: *firing,
                node,
                outcome,
            })
        }
        Event::RouteApplied(applied) => Some(FoldEvent::RouteApplied {
            firing:   applied.firing(),
            decision: applied.decision(),
        }),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecisionAddress {
    pub invocation: InvocationId,
    pub execution:  ExecutionId,
    pub decision:   DecisionId,
}

#[derive(Clone, Debug)]
pub struct AdmitCall {
    pub address: DecisionAddress,
    pub state:   Value,
}

#[derive(Clone, Debug)]
pub struct RouteCall {
    pub address:  DecisionAddress,
    pub firing:   FiringId,
    pub proposal: Arc<RoutingProposal>,
    pub state:    Value,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct MiddlewareError {
    message: SmolStr,
}

impl MiddlewareError {
    pub fn new(message: impl Into<SmolStr>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

type DecisionFuture<D> = Pin<Box<dyn Future<Output = Result<D, MiddlewareError>> + Send>>;
type LayerFuture<D, T> =
    Pin<Box<dyn Future<Output = Result<Resolved<D, T>, MiddlewareError>> + Send>>;

/// The rest of the chain below one layer, as the layer calls it.
pub struct Next<D> {
    call: Arc<dyn Fn() -> DecisionFuture<D> + Send + Sync>,
}

impl<D> Clone for Next<D> {
    fn clone(&self) -> Self {
        Self {
            call: self.call.clone(),
        }
    }
}

impl<D> Next<D> {
    pub async fn run(&self) -> Result<D, MiddlewareError> {
        (self.call)().await
    }
}

pub type AdmitNext = Next<Admission>;
pub type RouteNext = Next<RouteDecision>;

#[async_trait::async_trait]
pub trait Middleware: Send + Sync {
    fn key(&self) -> MiddlewareKey;
    fn state_version(&self) -> u32;
    fn initial_state(&self) -> Value;

    fn fold(&self, state: &mut Value, event: &FoldEvent<'_>) -> Result<(), MiddlewareError>;

    async fn admit(&self, _call: AdmitCall, next: AdmitNext) -> Result<Admission, MiddlewareError> {
        next.run().await
    }

    async fn route(
        &self,
        _call: RouteCall,
        next: RouteNext,
    ) -> Result<RouteDecision, MiddlewareError> {
        next.run().await
    }
}

pub fn initial_middleware_state(chain: &[Arc<dyn Middleware>]) -> MiddlewareState {
    chain
        .iter()
        .map(|middleware| {
            (
                middleware.key(),
                (middleware.state_version(), middleware.initial_state()),
            )
        })
        .collect()
}

pub fn validate_middleware_state(
    chain: &[Arc<dyn Middleware>],
    state: &MiddlewareState,
) -> Result<(), MiddlewareError> {
    for middleware in chain {
        let key = middleware.key();
        let Some((version, _)) = state.get(&key) else {
            return Err(MiddlewareError::new(format!(
                "middleware state for `{key}` is missing"
            )));
        };
        if *version != middleware.state_version() {
            return Err(MiddlewareError::new(format!(
                "middleware `{key}` state version is {version}; expected {}",
                middleware.state_version()
            )));
        }
    }
    if state.len() != chain.len() {
        return Err(MiddlewareError::new(
            "middleware state contains an unconfigured key",
        ));
    }
    Ok(())
}

/// A chain layer's decision with the interventions recorded below it.
#[derive(Clone)]
struct Resolved<D, T> {
    decision: D,
    trace:    Vec<T>,
}

/// Combine a layer's decision with the downstream result: the downstream trace
/// carries forward, and the layer is prepended only when it changed the
/// decision.
fn resolve_layer<D: PartialEq, T>(
    downstream: Option<Resolved<D, T>>,
    decision: D,
    entry: impl FnOnce(&D) -> T,
) -> Resolved<D, T> {
    let diverged = downstream
        .as_ref()
        .is_none_or(|value| value.decision != decision);
    let mut trace = downstream.map_or_else(Vec::new, |value| value.trace);
    if diverged {
        trace.insert(0, entry(&decision));
    }
    Resolved { decision, trace }
}

/// One execution-bound middleware chain and its externally owned state.
pub struct MiddlewarePipeline {
    invocation: InvocationId,
    execution:  ExecutionId,
    chain:      Arc<[Arc<dyn Middleware>]>,
    state:      Arc<RwLock<MiddlewareState>>,
}

impl MiddlewarePipeline {
    pub fn new(
        invocation: InvocationId,
        execution: ExecutionId,
        chain: Vec<Arc<dyn Middleware>>,
        state: MiddlewareState,
    ) -> Result<Self, MiddlewareError> {
        validate_middleware_state(&chain, &state)?;
        Ok(Self {
            invocation,
            execution,
            chain: Arc::from(chain),
            state: Arc::new(RwLock::new(state)),
        })
    }

    pub fn checkpoint(&self) -> MiddlewareState {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn fold_observer(&self) -> MiddlewareFoldObserver {
        MiddlewareFoldObserver {
            chain:   self.chain.clone(),
            state:   self.state.clone(),
            failure: Arc::new(Mutex::new(None)),
        }
    }

    pub fn fold(&self, event: &FoldEvent<'_>) -> Result<(), MiddlewareError> {
        let mut states = self.state.write().unwrap_or_else(PoisonError::into_inner);
        fold_states(&self.chain, &mut states, event)
    }

    fn address(&self, decision: DecisionId) -> DecisionAddress {
        DecisionAddress {
            invocation: self.invocation,
            execution: self.execution,
            decision,
        }
    }
}

#[async_trait::async_trait]
impl DecisionResolver for MiddlewarePipeline {
    async fn admit(&self, request: AdmitRequest) -> Result<AdmissionResolution, DecisionError> {
        let resolved = admit_at(
            self.chain.clone(),
            self.state.clone(),
            self.address(request.decision_id),
        )
        .await
        .map_err(|error| DecisionError::new(error.message()))?;
        Ok(AdmissionResolution {
            decision: resolved.decision,
            trace:    resolved.trace,
        })
    }

    async fn route(&self, request: RoutingRequest) -> Result<RoutingResolution, DecisionError> {
        let DecisionId::Route { firing, .. } = request.decision_id else {
            return Err(DecisionError::new(
                "a routing request must carry a route decision id",
            ));
        };
        let mut groups = Vec::with_capacity(request.groups.len());
        for proposal in request.groups {
            let baseline = default_group_decision(&proposal, request.restart_allowed)?;
            let proposal = Arc::new(proposal);
            let resolved = route_at(
                self.chain.clone(),
                self.state.clone(),
                self.address(request.decision_id),
                firing,
                proposal.clone(),
                baseline.decision,
            )
            .await
            .map_err(|error| DecisionError::new(error.message()))?;
            let decision = engine::enforce_restart_limit(
                request.restart_allowed,
                &proposal,
                resolved.decision,
            );
            groups.push(GroupDecision {
                group: proposal.group,
                draw: baseline.draw,
                trace: resolved.trace,
                decision,
            });
        }
        Ok(RoutingResolution { groups })
    }

    fn admit_now(&self, _request: &AdmitRequest) -> Option<AdmissionResolution> {
        self.chain.is_empty().then(|| AdmissionResolution {
            decision: Admission::Admit,
            trace:    Vec::new(),
        })
    }

    fn route_now(&self, request: &RoutingRequest) -> Option<RoutingResolution> {
        if !self.chain.is_empty() {
            return None;
        }
        let groups = request
            .groups
            .iter()
            .map(|proposal| default_group_decision(proposal, request.restart_allowed))
            .collect::<Result<_, _>>()
            .ok()?;
        Some(RoutingResolution { groups })
    }
}

/// One layer's call: build the typed call payload and invoke the layer's
/// trait method with the rest of the chain behind `next`.
type Invoke<D> = dyn Fn(Arc<dyn Middleware>, Value, Next<D>) -> DecisionFuture<D> + Send + Sync;

/// How a layer's decision renders into the trace.
type TraceEntry<D, T> = dyn Fn(MiddlewareKey, &D) -> T + Send + Sync;

fn admit_at(
    chain: Arc<[Arc<dyn Middleware>]>,
    state: Arc<RwLock<MiddlewareState>>,
    address: DecisionAddress,
) -> LayerFuture<Admission, MiddlewareKey> {
    layer_at(
        0,
        chain,
        state,
        Admission::Admit,
        Arc::new(move |middleware, state, next| {
            Box::pin(async move { middleware.admit(AdmitCall { address, state }, next).await })
        }),
        Arc::new(|key, _| key),
    )
}

fn route_at(
    chain: Arc<[Arc<dyn Middleware>]>,
    state: Arc<RwLock<MiddlewareState>>,
    address: DecisionAddress,
    firing: FiringId,
    proposal: Arc<RoutingProposal>,
    baseline: RouteDecision,
) -> LayerFuture<RouteDecision, Intervention> {
    layer_at(
        0,
        chain,
        state,
        baseline,
        Arc::new(move |middleware, state, next| {
            let proposal = proposal.clone();
            Box::pin(async move {
                middleware
                    .route(
                        RouteCall {
                            address,
                            firing,
                            proposal,
                            state,
                        },
                        next,
                    )
                    .await
            })
        }),
        Arc::new(intervention),
    )
}

/// The chain recursion both decision kinds share: resolve the layer at
/// `index`, giving it the rest of the chain as `next` and capturing what the
/// downstream layers resolved so the trace composes.
fn layer_at<D, T>(
    index: usize,
    chain: Arc<[Arc<dyn Middleware>]>,
    state: Arc<RwLock<MiddlewareState>>,
    default: D,
    invoke: Arc<Invoke<D>>,
    entry: Arc<TraceEntry<D, T>>,
) -> LayerFuture<D, T>
where
    D: Clone + PartialEq + Send + Sync + 'static,
    T: Send + 'static,
{
    Box::pin(async move {
        let Some(middleware) = chain.get(index).cloned() else {
            return Ok(Resolved {
                decision: default,
                trace:    Vec::new(),
            });
        };
        let captured = Arc::new(Mutex::new(None));
        let next = Next {
            call: Arc::new({
                let chain = chain.clone();
                let state = state.clone();
                let captured = captured.clone();
                let default = default.clone();
                let invoke = invoke.clone();
                let entry = entry.clone();
                move || {
                    let chain = chain.clone();
                    let state = state.clone();
                    let captured = captured.clone();
                    let default = default.clone();
                    let invoke = invoke.clone();
                    let entry = entry.clone();
                    Box::pin(async move {
                        let resolved =
                            layer_at(index + 1, chain, state, default, invoke, entry).await?;
                        let decision = resolved.decision.clone();
                        *captured.lock().unwrap_or_else(PoisonError::into_inner) = Some(resolved);
                        Ok(decision)
                    })
                }
            }),
        };
        let key = middleware.key();
        let middleware_state = read_state(&state, &key)?;
        let decision = invoke(middleware, middleware_state, next).await?;
        let downstream = captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        Ok(resolve_layer(downstream, decision, |decision| {
            entry(key, decision)
        }))
    })
}

fn read_state(
    state: &RwLock<MiddlewareState>,
    key: &MiddlewareKey,
) -> Result<Value, MiddlewareError> {
    state
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key)
        .map(|(_, state)| state.clone())
        .ok_or_else(|| MiddlewareError::new(format!("middleware state for `{key}` is missing")))
}

fn intervention(key: MiddlewareKey, decision: &RouteDecision) -> Intervention {
    match decision {
        RouteDecision::Emit(edge) => Intervention::Override {
            middleware: key,
            edge:       *edge,
        },
        RouteDecision::Jump(target) => Intervention::Jump {
            middleware: key,
            target:     *target,
        },
        RouteDecision::Block { reason } => Intervention::Block {
            middleware: key,
            reason:     reason.clone(),
        },
        RouteDecision::None => Intervention::Block {
            middleware: key,
            reason:     SmolStr::new("middleware selected no route"),
        },
    }
}

#[derive(Clone)]
pub struct MiddlewareFoldObserver {
    chain:   Arc<[Arc<dyn Middleware>]>,
    state:   Arc<RwLock<MiddlewareState>>,
    failure: Arc<Mutex<Option<MiddlewareError>>>,
}

#[async_trait::async_trait]
impl driver::EventObserver for MiddlewareFoldObserver {
    fn on_record(&self, record: &EventRecord, state: &engine::EngineState) {
        if self.chain.is_empty() {
            return;
        }
        // Finality: `apply` records the final attempt in history in the same
        // transition, so the matching entry sits at (or near) the tail.
        let fold = derive_fold_event(
            &record.event,
            |firing, attempt| {
                state
                    .history()
                    .iter()
                    .rev()
                    .any(|entry| entry.firing == firing && entry.attempt == attempt)
            },
            |firing| state.firing_node(firing),
        );
        if let Some(event) = fold {
            self.apply_fold(&event);
        }
    }

    async fn finish(&self) -> Result<(), driver::ObserveError> {
        match self
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            Some(error) => Err(driver::ObserveError::new(
                "middleware fold",
                error.to_string(),
            )),
            None => Ok(()),
        }
    }
}

impl MiddlewareFoldObserver {
    fn apply_fold(&self, event: &FoldEvent<'_>) {
        let mut states = self.state.write().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = fold_states(&self.chain, &mut states, event) {
            *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
        }
    }
}

/// Fold one event into every layer's state, in chain order. The one loop both
/// the pipeline's direct fold and the live observer share.
fn fold_states(
    chain: &[Arc<dyn Middleware>],
    states: &mut MiddlewareState,
    event: &FoldEvent<'_>,
) -> Result<(), MiddlewareError> {
    for middleware in chain {
        let key = middleware.key();
        let (_, value) = states.get_mut(&key).ok_or_else(|| {
            MiddlewareError::new(format!("middleware state for `{key}` is missing"))
        })?;
        middleware.fold(value, event)?;
    }
    Ok(())
}
