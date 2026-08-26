//! A small builder for graphs. Frontends and tests use it so node ids stay equal to
//! their index and edge ids stay unique, which validation requires.
//!
//! The builder has no `connect` that grows a group: routing is set with one of
//! [`GraphBuilder::link`], [`GraphBuilder::select`] or [`GraphBuilder::fan_out`], so
//! fan-out is as explicit here as it is in the IR.

use serde_json::Value;

use crate::expr::ExprTable;
use crate::graph::{
    Budget, Edge, Expansion, Fallthrough, Graph, JoinPolicy, Node, Routing, Scope, SelectGroup,
    StepRef,
};
use crate::ids::{EdgeId, ExprId, NodeId, ScopeId, StepKindId};

/// One arm of a select group, before edge ids are allocated.
pub struct Arm {
    pub to: NodeId,
    pub guard: Option<ExprId>,
    pub map: Option<ExprId>,
    pub back: bool,
}

impl Arm {
    /// An unconditional arm. Only valid as a group's last arm.
    pub fn always(to: NodeId) -> Self {
        Self {
            to,
            guard: None,
            map: None,
            back: false,
        }
    }

    pub fn when(to: NodeId, guard: ExprId) -> Self {
        Self {
            to,
            guard: Some(guard),
            map: None,
            back: false,
        }
    }

    pub fn with_map(mut self, map: ExprId) -> Self {
        self.map = Some(map);
        self
    }

    pub fn as_back(mut self) -> Self {
        self.back = true;
        self
    }
}

#[derive(Debug, Default)]
pub struct GraphBuilder {
    graph: Graph,
    next_edge: u32,
}

impl GraphBuilder {
    /// A builder with one default scope, `ScopeId(0)`.
    pub fn new() -> Self {
        let mut builder = Self::default();
        builder.add_scope(Scope::new(ScopeId::new(0)));
        builder
    }

    /// A builder with no scopes at all, for callers that define their own.
    pub fn bare() -> Self {
        Self::default()
    }

    pub fn exprs(&mut self) -> &mut ExprTable {
        &mut self.graph.exprs
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    pub fn graph_mut(&mut self) -> &mut Graph {
        &mut self.graph
    }

    pub fn add_scope(&mut self, scope: Scope) -> ScopeId {
        let id = ScopeId::new(self.graph.scopes.len() as u32);
        let mut scope = scope;
        scope.id = id;
        self.graph.scopes.push(scope);
        id
    }

    /// Add a node with terminal routing; wire it up afterwards.
    pub fn add_node(&mut self, name: &str, scope: ScopeId, step: StepRef) -> NodeId {
        let id = NodeId::new(self.graph.nodes.len() as u32);
        self.graph.nodes.push(Node::new(id, name, scope, step));
        id
    }

    /// Add a node whose step is a bare kind with no config.
    pub fn add_step(&mut self, name: &str, scope: ScopeId, kind: impl Into<StepKindId>) -> NodeId {
        self.add_node(name, scope, StepRef::new(kind, Value::Null))
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        self.graph
            .node_mut(id)
            .expect("builder handed out a valid node id")
    }

    pub fn set_join(&mut self, node: NodeId, join: JoinPolicy) {
        self.node_mut(node).join = join;
    }

    pub fn set_budget(&mut self, node: NodeId, budget: Budget) {
        self.node_mut(node).budget = budget;
    }

    pub fn set_precondition(&mut self, node: NodeId, expr: ExprId) {
        self.node_mut(node).precondition = Some(expr);
    }

    pub fn set_expansion(&mut self, node: NodeId, expand: Expansion) {
        self.node_mut(node).expand = Some(expand);
    }

    pub fn mark_entry(&mut self, node: NodeId) {
        self.graph.entry.push(node);
    }

    pub fn next_edge_id(&mut self) -> EdgeId {
        let id = EdgeId::new(self.next_edge);
        self.next_edge += 1;
        id
    }

    /// Sequential `next:` — one group, one unconditional arm.
    pub fn link(&mut self, from: NodeId, to: NodeId) -> EdgeId {
        let id = self.next_edge_id();
        self.node_mut(from).routing = Routing::next(Edge::always(id, to));
        id
    }

    /// One group of ordered arms: exactly one successor is chosen.
    pub fn select(&mut self, from: NodeId, arms: Vec<Arm>) -> Vec<EdgeId> {
        self.select_with(from, arms, Fallthrough::NoEmit)
    }

    pub fn select_with(
        &mut self,
        from: NodeId,
        arms: Vec<Arm>,
        fallthrough: Fallthrough,
    ) -> Vec<EdgeId> {
        let (edges, ids) = self.build_arms(arms);
        self.node_mut(from).routing = Routing::groups(vec![SelectGroup {
            arms: edges,
            fallthrough,
        }]);
        ids
    }

    /// Explicit AND-split: one group per target, all emitting concurrently.
    pub fn fan_out(&mut self, from: NodeId, targets: &[NodeId]) -> Vec<EdgeId> {
        let arms = targets.iter().map(|t| Arm::always(*t)).collect();
        let (edges, ids) = self.build_arms(arms);
        self.node_mut(from).routing = Routing::fan_out(edges);
        ids
    }

    /// Fan-out where each group carries its own guard: a group with no matching arm
    /// emits nothing, which is the OR-split.
    pub fn fan_out_groups(&mut self, from: NodeId, groups: Vec<Vec<Arm>>) -> Vec<Vec<EdgeId>> {
        let mut built = Vec::with_capacity(groups.len());
        let mut ids = Vec::with_capacity(groups.len());
        for arms in groups {
            let (edges, group_ids) = self.build_arms(arms);
            built.push(SelectGroup::new(edges));
            ids.push(group_ids);
        }
        self.node_mut(from).routing = Routing::groups(built);
        ids
    }

    fn build_arms(&mut self, arms: Vec<Arm>) -> (Vec<Edge>, Vec<EdgeId>) {
        let mut edges = Vec::with_capacity(arms.len());
        let mut ids = Vec::with_capacity(arms.len());
        for arm in arms {
            let id = self.next_edge_id();
            ids.push(id);
            let mut edge = match arm.guard {
                Some(guard) => Edge::when(id, arm.to, guard),
                None => Edge::always(id, arm.to),
            };
            edge.map = arm.map;
            edge.back = arm.back;
            edges.push(edge);
        }
        (edges, ids)
    }

    /// Finish. Entry defaults to every node with no incoming edges when none was
    /// marked, which is what a linear frontend wants.
    pub fn build(mut self) -> Graph {
        if self.graph.entry.is_empty() {
            let entries: Vec<NodeId> = self
                .graph
                .nodes
                .iter()
                .map(|n| n.id)
                .filter(|id| self.graph.in_degree(*id) == 0)
                .collect();
            self.graph.entry = entries;
        }
        self.graph
    }
}
