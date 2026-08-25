//! The token-flow graph: nodes, explicit routing, joins, scopes.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use crate::expr::ExprTable;
use crate::ids::{EdgeId, ExprId, NodeId, ScopeId, StepKindId};

// ── Guards & edges ────────────────────────────────────────────────────────

/// Condition on an edge arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Guard {
    /// Always passes. May only appear as a group's final arm (invariant 2).
    Always,
    /// Boolean expression over the completing node's outcome and contexts.
    Expr(ExprId),
}

/// One arm of a select group: where a token goes, and when.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub to: NodeId,
    pub guard: Guard,
    /// Payload for the emitted token; `None` means the source outcome's `output`.
    pub map: Option<ExprId>,
    /// Back edge: traversal increments the token's `Generation`.
    /// Every cycle must contain at least one (invariant 1).
    pub back: bool,
}

impl Edge {
    /// An unconditional edge carrying the source output.
    pub fn always(id: EdgeId, to: NodeId) -> Self {
        Self {
            id,
            to,
            guard: Guard::Always,
            map: None,
            back: false,
        }
    }

    /// A guarded edge carrying the source output.
    pub fn when(id: EdgeId, to: NodeId, guard: ExprId) -> Self {
        Self {
            id,
            to,
            guard: Guard::Expr(guard),
            map: None,
            back: false,
        }
    }

    pub fn with_map(mut self, map: ExprId) -> Self {
        self.map = Some(map);
        self
    }

    /// Mark this edge as a back edge, so crossing it bumps the generation.
    pub fn as_back(mut self) -> Self {
        self.back = true;
        self
    }
}

// ── Routing: AND of XORs ──────────────────────────────────────────────────

/// One XOR-select: arms are tried in order and **at most one** token is emitted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelectGroup {
    /// Ordered; the first arm whose guard passes wins.
    pub arms: Vec<Edge>,
    pub fallthrough: Fallthrough,
}

impl SelectGroup {
    /// A group that emits nothing when no arm matches (OR-split, loop exit).
    pub fn new(arms: Vec<Edge>) -> Self {
        Self {
            arms,
            fallthrough: Fallthrough::NoEmit,
        }
    }

    /// A group that must match: used by frontends requiring totality.
    pub fn total(arms: Vec<Edge>) -> Self {
        Self {
            arms,
            fallthrough: Fallthrough::Error,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fallthrough {
    /// No arm matched -> emit nothing.
    #[default]
    NoEmit,
    /// No arm matched -> run error.
    Error,
}

/// A node's routing: an AND of XORs.
///
/// `groups.len() == 1` is pure selection, the default. `groups.len() > 1` is an
/// explicit fan-out: the groups emit concurrently. Fan-out is never implicit —
/// it takes writing more than one group.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Routing {
    pub groups: Vec<SelectGroup>,
}

impl Routing {
    /// Terminal node: no outgoing tokens.
    pub fn terminal() -> Self {
        Self { groups: Vec::new() }
    }

    /// One group of one unconditional arm — a plain `next:`.
    pub fn next(edge: Edge) -> Self {
        Self {
            groups: vec![SelectGroup::new(vec![edge])],
        }
    }

    /// One group of several guarded arms — pick exactly one successor.
    pub fn select(arms: Vec<Edge>) -> Self {
        Self {
            groups: vec![SelectGroup::new(arms)],
        }
    }

    /// One group per edge — an explicit AND-split.
    pub fn fan_out(edges: Vec<Edge>) -> Self {
        Self {
            groups: edges
                .into_iter()
                .map(|e| SelectGroup::new(vec![e]))
                .collect(),
        }
    }

    pub fn groups(groups: Vec<SelectGroup>) -> Self {
        Self { groups }
    }

    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.groups.iter().flat_map(|g| g.arms.iter())
    }
}

// ── Joins ─────────────────────────────────────────────────────────────────

/// How incoming tokens are matched into a firing. Tokens are matched per
/// `(node, generation)`; incoming edges are counted **as of firing time**, so edges
/// spliced in by an expansion are included.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinPolicy {
    /// A token present on every incoming edge of the same generation.
    #[default]
    All,
    /// The first token fires the node; later same-generation tokens are dropped.
    Any,
    /// Tokens on `n` distinct incoming edges of the same generation.
    Quorum { n: u32 },
}

// ── Nodes ─────────────────────────────────────────────────────────────────

/// What a node runs, and with what configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepRef {
    pub kind: StepKindId,
    /// May embed unresolved expression placeholders in HIR (see [`Expansion`]).
    pub config: Value,
}

impl StepRef {
    pub fn new(kind: StepKindId, config: Value) -> Self {
        Self { kind, config }
    }
}

/// Hard limits on a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Hard cap on firings of this node across all generations. Must be >= 1.
    pub max_firings: u32,
    pub timeout: Duration,
}

impl Budget {
    pub const UNBOUNDED_FIRINGS: u32 = u32::MAX;

    pub fn new(max_firings: u32, timeout: Duration) -> Self {
        Self {
            max_firings,
            timeout,
        }
    }

    /// A node that fires once, with a one-hour ceiling.
    pub fn once() -> Self {
        Self::new(1, Duration::from_secs(3600))
    }

    /// A node inside a loop: capped at `max_firings` iterations.
    pub fn looped(max_firings: u32) -> Self {
        Self::new(max_firings, Duration::from_secs(3600))
    }

    /// Whether the firing cap is finite (invariant 4 for looped nodes).
    pub fn is_finite(&self) -> bool {
        self.max_firings < Self::UNBOUNDED_FIRINGS
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::once()
    }
}

/// Parallel `for_each` / matrix.
///
/// Sequential `for_each` is **not** an expansion: it desugars to a cycle over back
/// edges and generations. The engine has no loop primitive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expansion {
    ForEach {
        /// Evaluates to an array at runtime; one clone per element, with `item` and
        /// `index` bound into the clone's expression context.
        items: ExprId,
        target: ExpandTarget,
        /// Scheduler admission control across the spliced clones.
        max_parallel: Option<u32>,
        /// The first clone failure cancels sibling clones, via the splice's cancel scope.
        fail_fast: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpandTarget {
    /// Clone this node only.
    Node,
    /// Clone the subgraph between `entry` and `exit` (loop bodies, matrix jobs).
    Subgraph { entry: NodeId, exit: NodeId },
}

/// A unit of work in the graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub name: SmolStr,
    pub scope: ScopeId,
    pub step: StepRef,
    pub join: JoinPolicy,
    /// Precondition evaluated in the node's own context. False means the node
    /// completes `Skipped` without executing; routing still runs, so `always()` and
    /// `failure()` guards downstream still see it.
    pub precondition: Option<ExprId>,
    pub routing: Routing,
    pub budget: Budget,
    /// HIR only; lowered away before execution.
    pub expand: Option<Expansion>,
}

impl Node {
    pub fn new(id: NodeId, name: &str, scope: ScopeId, step: StepRef) -> Self {
        Self {
            id,
            name: SmolStr::new(name),
            scope,
            step,
            join: JoinPolicy::All,
            precondition: None,
            routing: Routing::terminal(),
            budget: Budget::once(),
            expand: None,
        }
    }

    pub fn with_join(mut self, join: JoinPolicy) -> Self {
        self.join = join;
        self
    }

    pub fn with_precondition(mut self, expr: ExprId) -> Self {
        self.precondition = Some(expr);
        self
    }

    pub fn with_routing(mut self, routing: Routing) -> Self {
        self.routing = routing;
        self
    }

    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    pub fn with_expansion(mut self, expand: Expansion) -> Self {
        self.expand = Some(expand);
        self
    }
}

// ── Scopes ────────────────────────────────────────────────────────────────

/// Either a literal value or an expression resolved at firing time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ExprOrValue {
    Value(Value),
    Expr(ExprId),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RuntimeSpec {
    HostProcess,
    Docker {
        image: SmolStr,
        #[serde(default)]
        args: Vec<SmolStr>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspacePolicy {
    /// One workspace shared by every node in the scope.
    #[default]
    Shared,
    /// A fresh workspace per node.
    PerNode,
}

/// "Job" generalized: a resource scope, not a sequence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scope {
    pub id: ScopeId,
    pub env: BTreeMap<SmolStr, ExprOrValue>,
    pub runtime: RuntimeSpec,
    pub workspace: WorkspacePolicy,
}

impl Scope {
    pub fn new(id: ScopeId) -> Self {
        Self {
            id,
            env: BTreeMap::new(),
            runtime: RuntimeSpec::HostProcess,
            workspace: WorkspacePolicy::Shared,
        }
    }

    pub fn with_env(mut self, key: &str, value: ExprOrValue) -> Self {
        self.env.insert(SmolStr::new(key), value);
        self
    }

    pub fn with_runtime(mut self, runtime: RuntimeSpec) -> Self {
        self.runtime = runtime;
        self
    }

    pub fn with_workspace(mut self, workspace: WorkspacePolicy) -> Self {
        self.workspace = workspace;
        self
    }
}

// ── Graph ─────────────────────────────────────────────────────────────────

/// A whole workflow. HIR and executable plans share this shape; a plan is a graph
/// that carries no [`Node::expand`] and no unresolved config placeholders.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub scopes: Vec<Scope>,
    pub exprs: ExprTable,
    /// Seeded with one `Generation(0)` token each.
    pub entry: Vec<NodeId>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.index())
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id.index())
    }

    pub fn scope(&self, id: ScopeId) -> Option<&Scope> {
        self.scopes.get(id.index())
    }

    /// Ids of the edges pointing at `node`, in node order. Joins count these.
    pub fn incoming(&self, node: NodeId) -> Vec<EdgeId> {
        let mut ids: Vec<EdgeId> = self
            .nodes
            .iter()
            .flat_map(|n| n.routing.edges())
            .filter(|e| e.to == node)
            .map(|e| e.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// How many distinct edges point at `node`.
    pub fn in_degree(&self, node: NodeId) -> usize {
        self.incoming(node).len()
    }

    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.nodes.iter().flat_map(|n| n.routing.edges())
    }

    pub fn edge(&self, id: EdgeId) -> Option<&Edge> {
        self.edges().find(|e| e.id == id)
    }

    /// The node an edge leaves from.
    pub fn edge_source(&self, id: EdgeId) -> Option<NodeId> {
        self.nodes
            .iter()
            .find(|n| n.routing.edges().any(|e| e.id == id))
            .map(|n| n.id)
    }

    /// Whether this graph is executable: no HIR-only fields left (invariant 6).
    pub fn is_plan(&self) -> bool {
        self.nodes.iter().all(|n| n.expand.is_none())
    }
}
