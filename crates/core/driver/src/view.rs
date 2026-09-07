//! Immutable views a host policy sees: which node, which visit, which
//! attempt, with the inputs and context as they stood, plus the branch role
//! the node plays in its graph.
//!
//! A view is a snapshot. It is built by the driver from the engine state at
//! one decision point and handed to a resolver or lifecycle callback by
//! shared reference, so a callback can read everything it needs and change
//! nothing. Visits and retry attempts are distinct here: `visit` counts the
//! node's firings in this execution, `attempt` counts tries within one
//! firing.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use engine::EngineState;
use ir::{Attempt, FiringId, Generation, Graph, Node, NodeId, RunContext, ScopeId, Token, Value};

/// One firing at one decision point, read-only.
#[derive(Clone, Debug)]
pub struct FiringView {
    pub firing:     FiringId,
    /// Which try this is within the firing, 1-based.
    pub attempt:    Attempt,
    pub generation: Generation,
    /// The node as it stands in the live graph. A copy, so a callback never
    /// borrows engine state.
    pub node:       Arc<Node>,
    /// Which firing of this node this is within the execution, 1-based. A
    /// loop that fires the node again advances it; a retry does not.
    pub visit:      u32,
    pub scope:      ScopeId,
    /// The tokens whose arrival fired the node.
    pub inputs:     Vec<Token>,
    /// The resolved step config for this attempt. Present at admission,
    /// where the config was just resolved; absent after completion.
    pub config:     Option<Value>,
    /// The run context as it stood: `kv` and node records.
    pub context:    RunContext,
    /// Where the node sits relative to the graph's fork and join points.
    pub branch:     BranchRole,
}

impl FiringView {
    /// The node's frontend metadata (`Node::meta`): label, source span,
    /// kind, anything the frontend attached. `Null` when none was.
    pub fn meta(&self) -> &Value {
        &self.node.meta
    }

    /// The node's instance name.
    pub fn node_name(&self) -> &str {
        &self.node.name
    }
}

/// A node's position relative to a fork: the node that splits into several
/// routing groups at once, the nodes on one of its branches, and the node
/// where the branches meet again.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum BranchRole {
    /// Not on any branch.
    None,
    /// The node whose routing emits on several groups at once.
    Fork { branches: u32 },
    /// Inside one branch of a fork.
    Member(BranchRef),
    /// Reached from more than one branch of the fork: where they join.
    Join { fork: NodeId },
}

/// One branch of a fork: the fork node and the routing group ordinal that
/// starts the branch. Static, so it is the same on every visit and after a
/// resume.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct BranchRef {
    pub fork:  NodeId,
    pub index: u32,
}

/// The `Node::meta` key a frontend sets when it lowered a branch into a graph
/// of its own: `{ "fork": <node id in the caller's graph>, "index": <n> }`.
/// The node then plays [`BranchRole::Member`] of that branch even though its
/// own graph has no fork, so hosts and events see the same role a branch that
/// stayed in the caller's graph would have.
pub const BRANCH_ROLE_META: &str = "branch_role";

/// The branch roles of every node in a graph, computed once per graph shape.
///
/// A fork is a node with two or more routing groups. Each group's arms start
/// one branch; a branch is the nodes reachable from those arms along forward
/// edges without passing through a node another group also reaches. The
/// first node several branches reach, entered from a branch member or from
/// the fork itself, is the join. Nodes after the join have no role from this
/// fork. Back edges and restart edges are not followed, so a loop around the
/// fork does not pull its predecessors into a branch. Nested forks keep the
/// outer assignment for nodes both forks classify. Expansion clones are not
/// analysed: a clone's role comes from its splice, which is the
/// coordinator's business.
#[derive(Clone, Debug, Default)]
pub struct BranchMap {
    roles: BTreeMap<NodeId, BranchRole>,
    nodes: usize,
}

impl BranchMap {
    pub fn of(graph: &Graph) -> Self {
        let mut roles: BTreeMap<NodeId, BranchRole> = BTreeMap::new();
        for node in &graph.nodes {
            if let Some(declared) = declared_role(&node.meta) {
                roles.insert(node.id, declared);
            }
        }
        for fork in &graph.nodes {
            let groups = &fork.routing.groups;
            if groups.len() < 2 {
                continue;
            }
            let mut reached_by: BTreeMap<NodeId, BTreeSet<u32>> = BTreeMap::new();
            for (index, group) in groups.iter().enumerate() {
                let index = u32::try_from(index).unwrap_or(u32::MAX);
                let mut queue: VecDeque<NodeId> = group
                    .arms
                    .iter()
                    .filter(|arm| forward(arm))
                    .map(|arm| arm.to)
                    .collect();
                let mut seen = BTreeSet::new();
                while let Some(node) = queue.pop_front() {
                    if node == fork.id || !seen.insert(node) {
                        continue;
                    }
                    reached_by.entry(node).or_default().insert(index);
                    let Some(next) = graph.node(node) else {
                        continue;
                    };
                    queue.extend(
                        next.routing
                            .edges()
                            .filter(|edge| forward(edge))
                            .map(|edge| edge.to),
                    );
                }
            }
            let shared: BTreeSet<NodeId> = reached_by
                .iter()
                .filter(|(_, groups)| groups.len() > 1)
                .map(|(node, _)| *node)
                .collect();
            // A join is entered from a branch member or from the fork. A
            // shared node entered only from other shared nodes is past the
            // join.
            let joins: BTreeSet<NodeId> = shared
                .iter()
                .copied()
                .filter(|node| {
                    graph.nodes.iter().any(|source| {
                        (source.id == fork.id
                            || (reached_by.contains_key(&source.id)
                                && !shared.contains(&source.id)))
                            && source
                                .routing
                                .edges()
                                .any(|edge| forward(edge) && edge.to == *node)
                    })
                })
                .collect();
            roles.entry(fork.id).or_insert(BranchRole::Fork {
                branches: u32::try_from(groups.len()).unwrap_or(u32::MAX),
            });
            for (node, groups) in reached_by {
                if groups.len() == 1 {
                    roles.entry(node).or_insert(BranchRole::Member(BranchRef {
                        fork:  fork.id,
                        index: groups.into_iter().next().unwrap_or(0),
                    }));
                } else if joins.contains(&node) {
                    roles
                        .entry(node)
                        .or_insert(BranchRole::Join { fork: fork.id });
                }
            }
        }
        Self {
            roles,
            nodes: graph.nodes.len(),
        }
    }

    pub fn role(&self, node: NodeId) -> BranchRole {
        self.roles.get(&node).cloned().unwrap_or(BranchRole::None)
    }

    /// Whether this map was computed over a graph of this many nodes; a
    /// splice grows the graph and invalidates the map.
    pub fn covers(&self, graph: &Graph) -> bool {
        self.nodes == graph.nodes.len()
    }
}

/// The role a frontend declared under [`BRANCH_ROLE_META`], if any.
fn declared_role(meta: &Value) -> Option<BranchRole> {
    let role = meta.get(BRANCH_ROLE_META)?;
    let fork = u32::try_from(role.get("fork")?.as_u64()?).ok()?;
    let index = u32::try_from(role.get("index")?.as_u64()?).ok()?;
    Some(BranchRole::Member(BranchRef {
        fork: NodeId::new(fork),
        index,
    }))
}

/// Whether an edge advances within the execution: not a back edge, not a
/// restart.
fn forward(edge: &ir::Edge) -> bool {
    !edge.back && edge.transition == ir::EdgeTransition::Continue
}

/// Build the view of a live firing from the engine state, with the config
/// the pending admission carries when there is one.
pub(crate) fn live_view(
    state: &EngineState,
    branches: &BranchMap,
    firing: FiringId,
    config: Option<Value>,
) -> Option<FiringView> {
    let live = state.firing(firing)?;
    let node = state.graph().node(live.node)?;
    Some(FiringView {
        firing,
        attempt: live.attempt,
        generation: live.generation,
        node: Arc::new(node.clone()),
        visit: state.firing_count(live.node),
        scope: live.scope,
        inputs: live.inputs.clone(),
        config,
        context: state.run_context().clone(),
        branch: branches.role(live.node),
    })
}

/// Build the view of a firing whose routing is pending: the firing is
/// retired, so everything comes from the routing snapshot.
pub(crate) fn routing_view(
    state: &EngineState,
    branches: &BranchMap,
    firing: FiringId,
) -> Option<FiringView> {
    let pending = state
        .pending_routings()
        .find(|pending| pending.firing == firing)?;
    Some(FiringView {
        firing,
        attempt: pending.attempt,
        generation: pending.generation,
        node: Arc::new(pending.node.clone()),
        visit: state.firing_count(pending.node.id),
        scope: pending.node.scope,
        inputs: pending.inputs.clone(),
        config: None,
        context: pending.run.clone(),
        branch: branches.role(pending.node.id),
    })
}
