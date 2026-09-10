//! Per-node routing: one tiered group with Fabro's four tiers, the failure
//! policy that guards the last one, and the retry policy.

use std::time::Duration;

use frontend::Diagnostics;
use ir::{
    Backoff, BinOp, Candidate, Edge, EdgeTransition, ExprId, GraphBuilder, Guard, NodeId,
    PickPolicy, RetryOn, RetryPolicy, RoutingGroup, SelectionPolicy, Tier, UnOp,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::kinds::RETRY_REQUESTED_CLASS;
use crate::model::{Attrs, NodeDecl, Workflow};

/// One of Fabro's failure policies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// The outcome stays failed and may take an unconditional edge.
    #[default]
    Route,
    /// The outcome stays failed and skips the unconditional edge, so the run
    /// ends unless a conditional edge or a retry target applies.
    Exit,
    /// The failure becomes a `PartialSuccess` that keeps the failure on the
    /// record, and routes as a success. As `on_retries_exhausted` (Fabro's
    /// `allow_partial=true`) it applies to the last retryable failure with no
    /// explicit-route check, as Fabro finalizes an exhausted retry; as
    /// `on_failure` it is a Petri extension with `succeed`'s route check.
    PartiallySucceed,
    /// Fabro's `succeed`: a failure that no explicit route matches is
    /// promoted. The step keeps the failure on its record as a partial
    /// status and reports `succeeded` to its edge conditions and to later
    /// stages, as Fabro shows them. A failure an explicit route matches (a
    /// condition, a preferred label, or a suggested target) stays failed and
    /// takes that route, as Fabro's executor orders it. The same check and
    /// promotion apply to a retryable failure once its attempts ran out.
    /// `auto_status=true` is the deprecated spelling.
    Succeed,
}

impl Policy {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "route" => Self::Route,
            "exit" => Self::Exit,
            "partially_succeed" => Self::PartiallySucceed,
            "succeed" => Self::Succeed,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Route => "route",
            Self::Exit => "exit",
            Self::PartiallySucceed => "partially_succeed",
            Self::Succeed => "succeed",
        }
    }

    /// Whether a still-failed outcome under this policy takes the
    /// unconditional edge.
    fn routes(self) -> bool {
        // A `partially_succeed` or `succeed` outcome that is still failed at
        // routing time is one the policy did not apply to; it routes like
        // `route`.
        matches!(self, Self::Route | Self::PartiallySucceed | Self::Succeed)
    }

    /// Whether this policy can turn a failure into a partial success, so the
    /// step applies it where it classifies its result.
    fn promotes(self) -> bool {
        matches!(self, Self::PartiallySucceed | Self::Succeed)
    }
}

/// The two policies of one node: what a non-retryable failure does, and what
/// running out of retries does. `allow_partial=true` is Fabro's spelling of
/// `on_retries_exhausted="partially_succeed"`. Both ride in the step config
/// (`on_failure`, `on_retries_exhausted`): the step applies the one that
/// decides its result, since Fabro's executor applies its failure policy to
/// the exhausted retry the same way as to an ordinary failure, and the
/// engine's own `Exhaustion` stays `Fail` for every Fabro node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailurePolicy {
    pub on_failure:           Policy,
    pub on_retries_exhausted: Policy,
}

impl FailurePolicy {
    /// The node's policies, with the graph's as defaults. Unknown spellings
    /// were diagnosed where the attributes were read and count as absent
    /// here; the one diagnostic this emits is an `allow_partial` that
    /// contradicts an explicit `on_retries_exhausted`.
    pub fn of(node: &NodeDecl, workflow: &Workflow, diags: &mut Diagnostics) -> Self {
        let read = |key: &str, attrs: &Attrs| attrs.text(key).and_then(|text| Policy::parse(&text));
        // Fabro reads `auto_status=true` as the node's `on_failure="succeed"`
        // when no explicit `on_failure` is set.
        let auto_status = node.attrs.bool("auto_status", diags).unwrap_or(false);
        let on_failure = read("on_failure", &node.attrs)
            .or_else(|| auto_status.then_some(Policy::Succeed))
            .or_else(|| read("on_failure", &workflow.attrs))
            .unwrap_or(Policy::Route);
        let allow_partial = node.attrs.bool("allow_partial", diags).unwrap_or(false);
        let exhausted = read("on_retries_exhausted", &node.attrs)
            .or_else(|| read("on_retries_exhausted", &workflow.attrs));
        let on_retries_exhausted = match (exhausted, allow_partial) {
            (Some(policy), true) if policy != Policy::PartiallySucceed => {
                diags.error(
                    "fabro.allow_partial_conflict",
                    node.attrs.span_of("allow_partial", &node.span),
                    format!(
                        "`allow_partial=true` means `on_retries_exhausted=\"partially_succeed\"`, \
                         but the node says `{}`",
                        policy.name()
                    ),
                );
                policy
            }
            (Some(policy), _) => policy,
            (None, true) => Policy::PartiallySucceed,
            (None, false) => on_failure,
        };
        Self {
            on_failure,
            on_retries_exhausted,
        }
    }

    /// Whether either policy is `succeed`, so a failure this node promotes
    /// reads as `succeeded` to its edge conditions.
    pub fn succeeds(self) -> bool {
        self.on_failure == Policy::Succeed || self.on_retries_exhausted == Policy::Succeed
    }

    /// Whether either policy can promote a failure, so the step needs the
    /// node's explicit routes to make Fabro's precheck.
    pub fn promotes(self) -> bool {
        self.on_failure.promotes() || self.on_retries_exhausted.promotes()
    }
}

/// One outgoing edge, lowered as far as routing needs.
pub(super) struct OutEdge {
    pub to:        NodeId,
    pub target:    String,
    pub condition: Option<ExprId>,
    pub label:     Option<String>,
    /// The static key a preferred label is compared against.
    pub label_key: Option<String>,
    /// As written: Fabro allows a negative weight to deprioritize an edge.
    /// [`group`] maps a node's weights onto the engine's unsigned ones.
    pub weight:    i64,
    pub restart:   bool,
    pub map:       Option<ExprId>,
}

/// The engine weight of each edge. Order and ties are Fabro's: when a node
/// has a negative weight, every weight shifts up so the lowest is zero. Under
/// random selection a weight at or below zero counts as one, as Fabro's
/// `weighted_random` counts it.
fn engine_weights(edges: &[OutEdge], random: bool) -> Vec<u32> {
    let floor = edges.iter().map(|e| e.weight).min().unwrap_or(0).min(0);
    edges
        .iter()
        .map(|e| {
            let weight = if random {
                e.weight.max(1)
            } else {
                e.weight - floor
            };
            u32::try_from(weight).unwrap_or(u32::MAX)
        })
        .collect()
}

/// `!(failure() || cancelled() || timed_out())`: the statuses Fabro folds
/// into `failed`, negated.
fn not_failed(b: &mut GraphBuilder) -> ExprId {
    let exprs = b.exprs();
    let mut failed = None;
    for tag in ["failure", "cancelled", "timed_out"] {
        let status = exprs.var("status");
        let want = exprs.lit(tag);
        let is = exprs.binary(BinOp::Eq, status, want);
        failed = Some(match failed {
            None => is,
            Some(acc) => exprs.binary(BinOp::Or, acc, is),
        });
    }
    let failed = failed.expect("three tags");
    exprs.unary(UnOp::Not, failed)
}

/// The tier-4 guard for a node's failure policy: a success-like or skipped
/// outcome always falls through; a failed one falls through only if the
/// policy that applies to it says `route`. Which policy applies is read off
/// the outcome: a failure the step classed `retry_requested` reached routing
/// only because the attempts ran out, so `on_retries_exhausted` decides;
/// any other failure is non-retryable and `on_failure` decides.
fn fallback_guard(b: &mut GraphBuilder, policy: FailurePolicy, human: bool) -> Guard {
    let route_f = policy.on_failure.routes() && !human;
    let route_x = policy.on_retries_exhausted.routes() && !human;
    if route_f && route_x {
        return Guard::Always;
    }
    let ok = not_failed(b);
    if !route_f && !route_x {
        return Guard::Expr(ok);
    }
    let exprs = b.exprs();
    let class = exprs.path("output", &["failure_class"]);
    let want = exprs.lit(RETRY_REQUESTED_CLASS);
    let retryable = exprs.binary(BinOp::Eq, class, want);
    let routes = if route_x {
        retryable
    } else {
        exprs.unary(UnOp::Not, retryable)
    };
    Guard::Expr(exprs.binary(BinOp::Or, ok, routes))
}

/// The routing group for one node.
pub(super) fn group(
    b: &mut GraphBuilder,
    edges: &[OutEdge],
    policy: FailurePolicy,
    human: bool,
    random: bool,
) -> RoutingGroup {
    let pick = if random {
        PickPolicy::WeightedRandom
    } else {
        PickPolicy::HighestWeightThenLexical
    };
    let truth = b.exprs().lit(true);
    let weights = engine_weights(edges, random);
    let mut arms = Vec::with_capacity(edges.len());
    let mut conditional = Vec::new();
    let mut labelled = Vec::new();
    let mut suggested = Vec::new();
    let mut fallback = Vec::new();
    for (edge, weight) in edges.iter().zip(weights) {
        let id = b.next_edge_id();
        // The tiers carry the real conditions; the arm's own guard is the
        // literal truth so invariant 2 (`Always` only last) holds whatever the
        // order.
        let mut arm = Edge::when(id, edge.to, truth).with_weight(weight);
        arm.map = edge.map;
        if let Some(label) = &edge.label {
            arm.label = Some(SmolStr::new(label));
        }
        if edge.restart {
            arm.transition = EdgeTransition::Restart;
        }
        arms.push(arm);
        if let Some(when) = edge.condition {
            conditional.push(Candidate {
                edge: id,
                when: Guard::Expr(when),
                rank: None,
            });
            continue;
        }
        if let Some(key) = &edge.label_key {
            let exprs = b.exprs();
            let reported = exprs.path("output", &["preferred_label"]);
            let empty = exprs.lit("");
            let present = exprs.call("default", vec![reported, empty]);
            let normalized = exprs.call("normalize_label", vec![present]);
            let want = exprs.lit(key.as_str());
            let when = exprs.binary(BinOp::Eq, normalized, want);
            labelled.push(Candidate {
                edge: id,
                when: Guard::Expr(when),
                rank: None,
            });
        }
        let exprs = b.exprs();
        let ids = exprs.path("output", &["suggested_next_ids"]);
        let none = exprs.array(Vec::new());
        let list = exprs.call("default", vec![ids, none]);
        let target = exprs.lit(edge.target.as_str());
        let index = exprs.call("index_of", vec![list, target]);
        let null = exprs.lit(serde_json::Value::Null);
        let when = exprs.binary(BinOp::Ne, index, null);
        suggested.push(Candidate {
            edge: id,
            when: Guard::Expr(when),
            rank: Some(index),
        });
        fallback.push(id);
    }
    let mut tiers = Vec::with_capacity(4);
    if !conditional.is_empty() {
        tiers.push(Tier {
            candidates: conditional,
            pick,
        });
    }
    if !labelled.is_empty() {
        tiers.push(Tier {
            candidates: labelled,
            pick:       PickPolicy::First,
        });
    }
    if !suggested.is_empty() {
        tiers.push(Tier {
            candidates: suggested,
            pick:       PickPolicy::LowestRankThenArmOrder,
        });
    }
    if !fallback.is_empty() {
        let when = fallback_guard(b, policy, human);
        tiers.push(Tier {
            candidates: fallback
                .into_iter()
                .map(|edge| Candidate {
                    edge,
                    when,
                    rank: None,
                })
                .collect(),
            pick,
        });
    }
    RoutingGroup::new(arms).with_policy(SelectionPolicy::Tiered(tiers))
}

const DEFAULT_BACKOFF: Backoff = Backoff {
    initial: Duration::from_secs(5),
    factor:  2.0,
    max:     Duration::from_secs(60),
    jitter:  true,
};

/// Fabro's retry policy for a node: a `retry_policy` preset, else
/// `max_retries` (default the graph's `default_max_retries`, default 0). Only
/// a `retry_requested` failure is retried: Fabro's retry intent is a flag on
/// the outcome, never a status. Exhaustion is the engine's `Fail`: what the
/// last retryable failure becomes is the step's decision under
/// `on_retries_exhausted`, made with the routes in hand, so the engine never
/// converts a failure an explicit route should have caught.
pub(super) fn retry_policy(
    node: &NodeDecl,
    workflow: &Workflow,
    diags: &mut Diagnostics,
) -> RetryPolicy {
    let preset = node.attrs.text("retry_policy");
    let (attempts, backoff) = match preset.as_deref() {
        Some("none") => (1, DEFAULT_BACKOFF),
        Some("standard") => (5, DEFAULT_BACKOFF),
        Some("aggressive") => (5, Backoff {
            initial: Duration::from_millis(500),
            ..DEFAULT_BACKOFF
        }),
        Some("linear") => (3, Backoff {
            initial: Duration::from_millis(500),
            factor: 1.0,
            ..DEFAULT_BACKOFF
        }),
        Some("patient") => (3, Backoff {
            initial: Duration::from_secs(2),
            factor: 3.0,
            ..DEFAULT_BACKOFF
        }),
        Some(other) => {
            diags.error(
                "fabro.bad_retry_policy",
                node.attrs.span_of("retry_policy", &node.span),
                format!(
                    "`retry_policy` must be `none`, `standard`, `aggressive`, `linear` or \
                     `patient`, not `{other}`"
                ),
            );
            (1, DEFAULT_BACKOFF)
        }
        None => {
            let retries = node
                .attrs
                .int("max_retries", diags)
                .or_else(|| workflow.attrs.int("default_max_retries", diags))
                .unwrap_or(0)
                .max(0);
            (
                u32::try_from(retries + 1).unwrap_or(u32::MAX),
                DEFAULT_BACKOFF,
            )
        }
    };
    RetryPolicy::attempts(attempts)
        .with_backoff(backoff)
        .with_retry_on(RetryOn::classes(&[RETRY_REQUESTED_CLASS]))
}
