use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use driver::{
    AdmissionResolution, AdmitRequest, DecisionError, DecisionResolver, DefaultDecisionResolver,
    RoutingRequest, RoutingResolution,
};
use engine::{
    Admission, DecisionId, Event, EventRecord, GroupDecision, Intervention, MiddlewareKey,
    RouteDecision, RoutingProposal,
};
use ir::{EdgeTransition, FiringId, NodeId, Outcome, Value};
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
        decision: &'a RouteDecision,
    },
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
    pub point:   engine::AdmitPoint,
    pub state:   Value,
}

#[derive(Clone, Debug)]
pub struct RouteCall {
    pub address:  DecisionAddress,
    pub firing:   FiringId,
    pub proposal: RoutingProposal,
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

type AdmitFuture = Pin<Box<dyn Future<Output = Result<Admission, MiddlewareError>> + Send>>;
type RouteFuture = Pin<Box<dyn Future<Output = Result<RouteDecision, MiddlewareError>> + Send>>;

#[derive(Clone)]
pub struct AdmitNext {
    call: Arc<dyn Fn() -> AdmitFuture + Send + Sync>,
}

impl AdmitNext {
    pub async fn run(&self) -> Result<Admission, MiddlewareError> {
        (self.call)().await
    }
}

#[derive(Clone)]
pub struct RouteNext {
    call: Arc<dyn Fn() -> RouteFuture + Send + Sync>,
}

impl RouteNext {
    pub async fn run(&self) -> Result<RouteDecision, MiddlewareError> {
        (self.call)().await
    }
}

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

#[derive(Clone)]
struct PipelineAdmission {
    decision: Admission,
    trace:    Vec<MiddlewareKey>,
}

#[derive(Clone)]
struct PipelineRoute {
    decision: RouteDecision,
    trace:    Vec<Intervention>,
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
        for middleware in self.chain.iter() {
            let key = middleware.key();
            let (_, value) = states.get_mut(&key).ok_or_else(|| {
                MiddlewareError::new(format!("middleware state for `{key}` is missing"))
            })?;
            middleware.fold(value, event)?;
        }
        Ok(())
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
            0,
            self.chain.clone(),
            self.state.clone(),
            self.address(request.decision_id),
            request.point,
        )
        .await
        .map_err(|error| DecisionError::new(error.message()))?;
        Ok(AdmissionResolution {
            decision: resolved.decision,
            trace:    resolved.trace,
        })
    }

    async fn route(&self, request: RoutingRequest) -> Result<RoutingResolution, DecisionError> {
        let baseline = DefaultDecisionResolver.route(request.clone()).await?;
        let mut groups = Vec::with_capacity(request.groups.len());
        for (proposal, baseline) in request.groups.into_iter().zip(baseline.groups) {
            let draw = baseline.draw;
            let resolved = route_at(
                0,
                self.chain.clone(),
                self.state.clone(),
                self.address(request.decision_id),
                request.firing,
                proposal.clone(),
                baseline.decision,
            )
            .await
            .map_err(|error| DecisionError::new(error.message()))?;
            let decision =
                enforce_restart_limit(request.restart_allowed, &proposal, resolved.decision);
            groups.push(GroupDecision {
                group: proposal.group,
                draw,
                trace: resolved.trace,
                decision,
            });
        }
        Ok(RoutingResolution { groups })
    }
}

fn admit_at(
    index: usize,
    chain: Arc<[Arc<dyn Middleware>]>,
    state: Arc<RwLock<MiddlewareState>>,
    address: DecisionAddress,
    point: engine::AdmitPoint,
) -> Pin<Box<dyn Future<Output = Result<PipelineAdmission, MiddlewareError>> + Send>> {
    Box::pin(async move {
        let Some(middleware) = chain.get(index).cloned() else {
            return Ok(PipelineAdmission {
                decision: Admission::Admit,
                trace:    Vec::new(),
            });
        };
        let captured = Arc::new(Mutex::new(None));
        let next_captured = captured.clone();
        let next_chain = chain.clone();
        let next_state = state.clone();
        let next = AdmitNext {
            call: Arc::new(move || {
                let chain = next_chain.clone();
                let state = next_state.clone();
                let captured = next_captured.clone();
                Box::pin(async move {
                    let resolved = admit_at(index + 1, chain, state, address, point).await?;
                    let decision = resolved.decision.clone();
                    *captured.lock().unwrap_or_else(PoisonError::into_inner) = Some(resolved);
                    Ok(decision)
                })
            }),
        };
        let key = middleware.key();
        let middleware_state = read_state(&state, &key)?;
        let decision = middleware
            .admit(
                AdmitCall {
                    address,
                    point,
                    state: middleware_state,
                },
                next,
            )
            .await?;
        let downstream = captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut trace = downstream
            .as_ref()
            .map_or_else(Vec::new, |value| value.trace.clone());
        if downstream
            .as_ref()
            .is_none_or(|value| value.decision != decision)
        {
            trace.insert(0, key);
        }
        Ok(PipelineAdmission { decision, trace })
    })
}

fn route_at(
    index: usize,
    chain: Arc<[Arc<dyn Middleware>]>,
    state: Arc<RwLock<MiddlewareState>>,
    address: DecisionAddress,
    firing: FiringId,
    proposal: RoutingProposal,
    baseline: RouteDecision,
) -> Pin<Box<dyn Future<Output = Result<PipelineRoute, MiddlewareError>> + Send>> {
    Box::pin(async move {
        let Some(middleware) = chain.get(index).cloned() else {
            return Ok(PipelineRoute {
                decision: baseline,
                trace:    Vec::new(),
            });
        };
        let captured = Arc::new(Mutex::new(None));
        let next_captured = captured.clone();
        let next_chain = chain.clone();
        let next_state = state.clone();
        let next_proposal = proposal.clone();
        let next_baseline = baseline.clone();
        let next = RouteNext {
            call: Arc::new(move || {
                let chain = next_chain.clone();
                let state = next_state.clone();
                let captured = next_captured.clone();
                let proposal = next_proposal.clone();
                let baseline = next_baseline.clone();
                Box::pin(async move {
                    let resolved =
                        route_at(index + 1, chain, state, address, firing, proposal, baseline)
                            .await?;
                    let decision = resolved.decision.clone();
                    *captured.lock().unwrap_or_else(PoisonError::into_inner) = Some(resolved);
                    Ok(decision)
                })
            }),
        };
        let key = middleware.key();
        let middleware_state = read_state(&state, &key)?;
        let decision = middleware
            .route(
                RouteCall {
                    address,
                    firing,
                    proposal: proposal.clone(),
                    state: middleware_state,
                },
                next,
            )
            .await?;
        let downstream = captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut trace = downstream
            .as_ref()
            .map_or_else(Vec::new, |value| value.trace.clone());
        if downstream
            .as_ref()
            .is_none_or(|value| value.decision != decision)
        {
            trace.insert(0, intervention(key, &decision));
        }
        Ok(PipelineRoute { decision, trace })
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

fn enforce_restart_limit(
    restart_allowed: bool,
    proposal: &RoutingProposal,
    decision: RouteDecision,
) -> RouteDecision {
    if restart_allowed {
        return decision;
    }
    match decision {
        RouteDecision::Emit(edge)
            if proposal.candidates.iter().any(|candidate| {
                candidate.edge == edge && candidate.transition == EdgeTransition::Restart
            }) =>
        {
            RouteDecision::Block {
                reason: SmolStr::new("maximum executions per invocation reached"),
            }
        }
        decision => decision,
    }
}

#[derive(Clone)]
pub struct MiddlewareFoldObserver {
    chain:   Arc<[Arc<dyn Middleware>]>,
    state:   Arc<RwLock<MiddlewareState>>,
    failure: Arc<Mutex<Option<MiddlewareError>>>,
}

impl MiddlewareFoldObserver {
    pub fn checkpoint(&self) -> MiddlewareState {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl driver::EventObserver for MiddlewareFoldObserver {
    fn on_record(&self, record: &EventRecord, state: &engine::EngineState) {
        let fold = match &record.event {
            Event::ExecutionStarted(_) | Event::RunStarted => Some(FoldEvent::ExecutionStarted),
            Event::StepFinished {
                firing,
                attempt,
                outcome,
            } if state
                .history()
                .iter()
                .any(|entry| entry.firing == *firing && entry.attempt == *attempt) =>
            {
                state
                    .firing_node(*firing)
                    .map(|node| FoldEvent::FinalOutcome {
                        firing: *firing,
                        node,
                        outcome,
                    })
            }
            Event::RouteApplied(applied) => {
                let (firing, decision) = match applied {
                    engine::RouteApplied::Edge { firing, edge, .. } => {
                        (*firing, RouteDecision::Emit(*edge))
                    }
                    engine::RouteApplied::Jump { firing, target } => {
                        (*firing, RouteDecision::Jump(*target))
                    }
                    engine::RouteApplied::None { firing, .. } => (*firing, RouteDecision::None),
                };
                self.fold_owned(firing, &decision);
                None
            }
            _ => None,
        };
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
    fn fold_owned(&self, firing: FiringId, decision: &RouteDecision) {
        self.apply_fold(&FoldEvent::RouteApplied { firing, decision });
    }

    fn apply_fold(&self, event: &FoldEvent<'_>) {
        let mut states = self.state.write().unwrap_or_else(PoisonError::into_inner);
        for middleware in self.chain.iter() {
            let key = middleware.key();
            let Some((_, value)) = states.get_mut(&key) else {
                *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(
                    MiddlewareError::new(format!("middleware state for `{key}` is missing")),
                );
                return;
            };
            if let Err(error) = middleware.fold(value, event) {
                *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
                return;
            }
        }
    }
}
