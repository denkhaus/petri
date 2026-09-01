//! Host-side admission and routing decisions.

use engine::{
    Admission, AdmitPoint, DecisionId, GroupDecision, MiddlewareKey, RouteDecision,
    RoutingProposal, WeightedDraw,
};
use ir::{EdgeTransition, PickPolicy};
use smol_str::SmolStr;

/// One admission command with its durable identity.
#[derive(Clone, Debug)]
pub struct AdmitRequest {
    pub point:       AdmitPoint,
    pub decision_id: DecisionId,
}

/// The result persisted in `Event::Admitted`.
#[derive(Clone, Debug)]
pub struct AdmissionResolution {
    pub decision: Admission,
    pub trace:    Vec<MiddlewareKey>,
}

/// One routing command with the core's proposals.
#[derive(Clone, Debug)]
pub struct RoutingRequest {
    pub firing:          ir::FiringId,
    pub decision_id:     DecisionId,
    pub restart_allowed: bool,
    pub groups:          Vec<RoutingProposal>,
}

/// The result persisted in `Event::RoutingResolved`.
#[derive(Clone, Debug)]
pub struct RoutingResolution {
    pub groups: Vec<GroupDecision>,
}

/// A decision pipeline failure. The driver converts it into a durable block.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DecisionError {
    message: SmolStr,
}

impl DecisionError {
    pub fn new(message: impl Into<SmolStr>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// The host extension point for admission and routing middleware.
#[async_trait::async_trait]
pub trait DecisionResolver: Send + Sync {
    async fn admit(&self, request: AdmitRequest) -> Result<AdmissionResolution, DecisionError>;

    async fn route(&self, request: RoutingRequest) -> Result<RoutingResolution, DecisionError>;
}

/// The ordinary Petri behavior with no user middleware.
#[derive(Default)]
pub struct DefaultDecisionResolver;

#[async_trait::async_trait]
impl DecisionResolver for DefaultDecisionResolver {
    async fn admit(&self, _request: AdmitRequest) -> Result<AdmissionResolution, DecisionError> {
        Ok(default_admission())
    }

    async fn route(&self, request: RoutingRequest) -> Result<RoutingResolution, DecisionError> {
        default_routing(request)
    }
}

pub(crate) fn default_admission() -> AdmissionResolution {
    AdmissionResolution {
        decision: Admission::Admit,
        trace:    Vec::new(),
    }
}

pub(crate) fn default_routing(request: RoutingRequest) -> Result<RoutingResolution, DecisionError> {
    let mut groups = Vec::with_capacity(request.groups.len());
    for proposal in request.groups {
        let (decision, draw) = choose(&proposal)?;
        let decision = match decision {
            RouteDecision::Emit(edge)
                if !request.restart_allowed
                    && proposal.candidates.iter().any(|candidate| {
                        candidate.edge == edge && candidate.transition == EdgeTransition::Restart
                    }) =>
            {
                RouteDecision::Block {
                    reason: SmolStr::new("maximum executions per invocation reached"),
                }
            }
            decision => decision,
        };
        groups.push(GroupDecision {
            group: proposal.group,
            draw,
            trace: Vec::new(),
            decision,
        });
    }
    Ok(RoutingResolution { groups })
}

fn choose(
    proposal: &RoutingProposal,
) -> Result<(RouteDecision, Option<WeightedDraw>), DecisionError> {
    let Some(pick) = proposal.pick else {
        return Ok((RouteDecision::None, None));
    };
    if proposal.candidates.is_empty() {
        return Ok((RouteDecision::None, None));
    }
    match pick {
        PickPolicy::First => Ok((RouteDecision::Emit(proposal.candidates[0].edge), None)),
        PickPolicy::HighestWeightThenLexical => {
            let mut winner = &proposal.candidates[0];
            for candidate in &proposal.candidates[1..] {
                if candidate.weight > winner.weight
                    || (candidate.weight == winner.weight && candidate.target < winner.target)
                {
                    winner = candidate;
                }
            }
            Ok((RouteDecision::Emit(winner.edge), None))
        }
        PickPolicy::LowestRankThenArmOrder => {
            let mut winner = None;
            for candidate in &proposal.candidates {
                let Some(rank) = candidate.rank else {
                    continue;
                };
                if winner.is_none_or(|current: &engine::RoutingCandidate| {
                    rank.total_cmp(&current.rank.unwrap_or(f64::INFINITY))
                        .is_lt()
                }) {
                    winner = Some(candidate);
                }
            }
            Ok((
                winner.map_or(RouteDecision::None, |candidate| {
                    RouteDecision::Emit(candidate.edge)
                }),
                None,
            ))
        }
        PickPolicy::WeightedRandom => {
            let total: u64 = proposal
                .candidates
                .iter()
                .map(|candidate| u64::from(candidate.weight))
                .sum();
            if total == 0 {
                return Err(DecisionError::new(
                    "weighted routing has no positive candidate weight",
                ));
            }
            let mut bytes = [0_u8; 8];
            getrandom::fill(&mut bytes)
                .map_err(|error| DecisionError::new(format!("random draw failed: {error}")))?;
            let roll = u64::from_le_bytes(bytes) % total;
            let draw = WeightedDraw {
                tier: proposal.tier.unwrap_or(0),
                candidates: proposal
                    .candidates
                    .iter()
                    .map(|candidate| candidate.edge)
                    .collect(),
                roll,
                total,
            };
            let mut cursor = roll;
            let selected = proposal
                .candidates
                .iter()
                .find(|candidate| {
                    let weight = u64::from(candidate.weight);
                    if cursor < weight {
                        true
                    } else {
                        cursor -= weight;
                        false
                    }
                })
                .ok_or_else(|| DecisionError::new("weighted draw selected no candidate"))?;
            Ok((RouteDecision::Emit(selected.edge), Some(draw)))
        }
    }
}
