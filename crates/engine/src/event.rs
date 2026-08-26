//! The engine's interface: what goes in ([`Event`]) and what comes out ([`Command`]).
//!
//! The core is sans-IO. It never runs a step, never reads a clock and never blocks.
//! A host turns commands into effects and feeds the results back as events.

use std::time::Duration;

use ir::{
    Attempt, CancelScopeId, Control, EdgeId, FiringId, Generation, Node, NodeId, Outcome,
    RunStatus, ScopeId, StepEvent, Token, Value,
};
use serde::{Deserialize, Serialize};

/// Something that happened. Every event is appended to the log before `apply`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    RunStarted,
    /// A token was placed on an edge. The core emits these for its own routing and
    /// seeding; a host may also inject one.
    TokenEmitted(Token),
    StepStarted {
        firing: FiringId,
        attempt: Attempt,
    },
    /// Logs, artifacts and step-defined progress. Carries no coordination meaning.
    StepProgress {
        firing: FiringId,
        ev: StepEvent,
    },
    StepFinished {
        firing: FiringId,
        attempt: Attempt,
        outcome: Outcome,
    },
    /// The driver waited out a retry's backoff. It applies jitter and does the
    /// sleeping; the core never sees a clock or an RNG.
    RetryElapsed {
        firing: FiringId,
        next_attempt: Attempt,
    },
    /// The result of a `for_each` expansion: clones spliced into the live graph.
    NodeExpanded {
        node: NodeId,
        splice: SubgraphSplice,
    },
    /// External cancellation. The run's root scope cancels everything.
    CancelRequested {
        scope: CancelScopeId,
    },
}

/// Everything an executor needs to run one step, with every expression already
/// resolved.
///
/// The type is the enforcement point for the boundary invariant: **no unresolved
/// expression placeholder crosses it, and a secret reference is the only non-literal
/// form that may.** Its fields are private and the only way to build one is
/// [`ResolvedFiring::new`]. Deserialization goes through the same check, so a value
/// read back off the wire carries the invariant too.
///
/// Secret references survive on purpose. A `ResolvedFiring` is serialized into the
/// event log, so resolving a secret here would write it to disk. `{"$secret": "NAME"}`
/// crosses instead, and the value is fetched at spawn time straight into the child's
/// environment. The reference must name a string; anything else is rejected here
/// rather than reaching a step. Whether a secret reference is in a position that
/// allows one is the step kind's business, not the boundary's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "ResolvedFiringRepr", try_from = "ResolvedFiringRepr")]
pub struct ResolvedFiring {
    id: FiringId,
    node: NodeId,
    generation: Generation,
    attempt: Attempt,
    scope: ScopeId,
    inputs: Vec<Token>,
    config: Value,
}

impl ResolvedFiring {
    /// Build one, refusing a config that still holds an expression placeholder.
    pub fn new(
        id: FiringId,
        node: NodeId,
        generation: Generation,
        attempt: Attempt,
        scope: ScopeId,
        inputs: Vec<Token>,
        config: Value,
    ) -> Result<Self, UnresolvedConfig> {
        if let Some(path) = ir::placeholder::placeholder_path(&config) {
            return Err(UnresolvedConfig {
                node,
                path,
                reason: BoundaryViolation::UnresolvedExpression,
            });
        }
        if let Some(path) = ir::placeholder::malformed_secret_ref(&config) {
            return Err(UnresolvedConfig {
                node,
                path,
                reason: BoundaryViolation::MalformedSecretRef,
            });
        }
        Ok(Self {
            id,
            node,
            generation,
            attempt,
            scope,
            inputs,
            config,
        })
    }

    pub fn id(&self) -> FiringId {
        self.id
    }

    pub fn node(&self) -> NodeId {
        self.node
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Which try this is, 1-based. The full identity of an attempt is
    /// `(node, generation, attempt)`.
    pub fn attempt(&self) -> Attempt {
        self.attempt
    }

    /// The resource scope the step runs in.
    pub fn scope(&self) -> ScopeId {
        self.scope
    }

    /// The tokens whose arrival satisfied the node's join.
    pub fn inputs(&self) -> &[Token] {
        &self.inputs
    }

    /// The step's configuration. Guaranteed free of expression placeholders.
    pub fn config(&self) -> &Value {
        &self.config
    }

    pub fn into_config(self) -> Value {
        self.config
    }
}

/// What was wrong at the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryViolation {
    /// An `{"$expr": id}` placeholder was never resolved.
    UnresolvedExpression,
    /// A `{"$secret": ...}` whose name is not a string.
    MalformedSecretRef,
}

impl std::fmt::Display for BoundaryViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoundaryViolation::UnresolvedExpression => f.write_str("an unresolved expression"),
            BoundaryViolation::MalformedSecretRef => {
                f.write_str("a secret reference whose name is not a string")
            }
        }
    }
}

/// A config reached the executor boundary in a state it may not cross in.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("step config for node {node:?} holds {reason} at `{path}`")]
pub struct UnresolvedConfig {
    pub node: NodeId,
    pub path: String,
    pub reason: BoundaryViolation,
}

/// The wire shape. Private, so the only public way in is through the checked
/// constructor.
#[derive(Serialize, Deserialize)]
struct ResolvedFiringRepr {
    id: FiringId,
    node: NodeId,
    generation: Generation,
    attempt: Attempt,
    scope: ScopeId,
    inputs: Vec<Token>,
    config: Value,
}

impl From<ResolvedFiring> for ResolvedFiringRepr {
    fn from(f: ResolvedFiring) -> Self {
        Self {
            id: f.id,
            node: f.node,
            generation: f.generation,
            attempt: f.attempt,
            scope: f.scope,
            inputs: f.inputs,
            config: f.config,
        }
    }
}

impl TryFrom<ResolvedFiringRepr> for ResolvedFiring {
    type Error = UnresolvedConfig;

    fn try_from(r: ResolvedFiringRepr) -> Result<Self, Self::Error> {
        ResolvedFiring::new(
            r.id,
            r.node,
            r.generation,
            r.attempt,
            r.scope,
            r.inputs,
            r.config,
        )
    }
}

/// Something the host must do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Run a step. The payload carries the config already resolved against the
    /// firing's context, so the host never reads the graph's unresolved copy.
    StartStep(ResolvedFiring),
    DeliverControl {
        firing: FiringId,
        ctl: Control,
    },
    /// Wait out `base_delay`, then feed back [`Event::RetryElapsed`].
    ///
    /// The delay is computed deterministically from the node's [`ir::Backoff`]. The
    /// driver adds jitter, which is why jitter lives there and not here.
    ScheduleRetry {
        firing: FiringId,
        next_attempt: Attempt,
        base_delay: Duration,
    },
    // reserved: external expansion. The core resolves `items` itself and splices in
    // the same `apply` call, so it never emits this. The variant is the seam for a
    // host that resolves items externally and feeds back `Event::NodeExpanded` — do
    // not delete it as dead code.
    ExpandNode {
        node: NodeId,
        generation: Generation,
        expr: ir::ExprId,
    },
    AcquireScope {
        scope: ScopeId,
    },
    ReleaseScope {
        scope: ScopeId,
    },
    FinishRun {
        status: RunStatus,
    },
}

/// One `for_each` element: a cloned node or subgraph, ready to splice in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpliceClone {
    /// Position in the `items` array; bound as `index` inside the clone.
    pub index: u32,
    /// The element itself; bound as `item` inside the clone.
    pub item: Value,
    /// Fully formed clone nodes. Their ids are already allocated in graph order.
    pub nodes: Vec<Node>,
    /// Where the clone starts. Seeded with `seed_edge`.
    pub entry: NodeId,
    /// Synthetic incoming edge for `entry`, so its join counts like any other.
    pub seed_edge: EdgeId,
}

/// The whole expansion: every clone, plus the cancel scope that covers them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubgraphSplice {
    /// A fresh cancel scope over the clones. `fail_fast` cancels this one.
    pub cancel_scope: CancelScopeId,
    /// The expanding node, which is the region's entry.
    pub source: NodeId,
    /// The original region the clones replace. Every node in it is superseded: it
    /// never executes, and its outgoing edges stop counting toward downstream
    /// joins, so a collector waits for the clones instead of the originals.
    pub region: Vec<NodeId>,
    /// Generation the expansion happened in; clones start there.
    pub generation: Generation,
    /// Payload the expanding node's join produced, seeded into every clone.
    pub payload: Value,
    pub clones: Vec<SpliceClone>,
    /// Admission control: at most this many clone firings run at once.
    pub max_parallel: Option<u32>,
    /// The first clone failure cancels the siblings.
    pub fail_fast: bool,
}

impl SubgraphSplice {
    /// Every node id this splice added.
    pub fn cloned_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.clones
            .iter()
            .flat_map(|c| c.nodes.iter().map(|n| n.id))
    }
}
