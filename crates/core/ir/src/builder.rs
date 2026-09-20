//! A small builder for graphs. Frontends and tests use it so node ids stay
//! equal to their index and edge ids stay unique, which validation requires.
//!
//! The builder has no `connect` that grows a group: routing is set with one of
//! [`GraphBuilder::link`], [`GraphBuilder::select`] or
//! [`GraphBuilder::fan_out`], so fan-out is as explicit here as it is in the
//! IR.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::expr::ExprTable;
use crate::graph::{
    Budget, Completion, Edge, Expansion, ExprOrValue, Fallthrough, Graph, GraphBody, Guard,
    JoinPolicy, Node, ResultProjection, Routing, RoutingGroup, RunPolicy, Scope, SelectGroup,
    SelectionPolicy, StepRef,
};
use crate::ids::{EdgeId, ExprId, Live, NodeId, ScopeId, StepKindId};
use crate::placeholder;

/// One arm of a select group, before edge ids are allocated.
pub struct Arm<S = Live> {
    pub to:    NodeId<S>,
    pub guard: Option<ExprId<S>>,
    pub map:   Option<ExprId<S>>,
    pub back:  bool,
}

impl<S> Arm<S> {
    /// An unconditional arm. Only valid as a group's last arm.
    pub fn always(to: NodeId<S>) -> Self {
        Self {
            to,
            guard: None,
            map: None,
            back: false,
        }
    }

    pub fn when(to: NodeId<S>, guard: ExprId<S>) -> Self {
        Self {
            to,
            guard: Some(guard),
            map: None,
            back: false,
        }
    }

    #[must_use]
    pub fn with_map(mut self, map: ExprId<S>) -> Self {
        self.map = Some(map);
        self
    }

    #[must_use]
    pub fn with_back(mut self) -> Self {
        self.back = true;
        self
    }
}

#[derive(Debug)]
pub struct GraphBuilder<S = Live> {
    graph:     Graph<S>,
    next_edge: u32,
}

impl<S> Default for GraphBuilder<S> {
    fn default() -> Self {
        Self {
            graph:     Graph {
                body:       GraphBody::default(),
                policy:     RunPolicy::default(),
                params:     BTreeMap::default(),
                completion: Completion::AnyFailure,
                result:     ResultProjection::None,
            },
            next_edge: 0,
        }
    }
}

impl GraphBuilder {
    /// A builder with one default scope, `ScopeId(0)`.
    pub fn new() -> Self {
        Self::with_default_scope()
    }

    /// A builder with no scopes at all, for callers that define their own.
    pub fn bare() -> Self {
        Self::default()
    }

    /// A builder holding a copy of `region`, nodes of `parent` in the order
    /// given, with the copies' ids by the originals'. The copy has the
    /// parent's scopes, with their environment expressions, and each node's
    /// step config, precondition, expansion, routing, budget, retry, flags
    /// and meta, every expression imported into the new table once. Each
    /// routing group keeps the arms whose target is in the region, in order,
    /// under the default selection policy; a group left with no arm is
    /// dropped, so no edge leaves the region. A cancel group is not copied.
    pub fn copy_region(parent: &Graph, region: &[NodeId]) -> (Self, HashMap<NodeId, NodeId>) {
        let mut copy = RegionCopy {
            parent,
            builder: Self::bare(),
            imported: HashMap::new(),
        };
        for scope in &parent.scopes {
            let mut scope = scope.clone();
            for value in scope.env.values_mut() {
                if let ExprOrValue::Expr(id) = value {
                    *id = copy.import(*id);
                }
            }
            copy.builder.add_scope(scope);
        }
        let mut copies: HashMap<NodeId, NodeId> = HashMap::new();
        for old in region {
            let Some(node) = parent.node(*old) else {
                continue;
            };
            let id =
                copy.builder
                    .add_node(&node.name, node.scope, StepRef::new("noop", Value::Null));
            copies.insert(*old, id);
        }
        for old in region {
            let Some(source) = parent.node(*old) else {
                continue;
            };
            copy.node(copies[old], source, &copies);
        }
        (copy.builder, copies)
    }
}

/// One region copy in progress: the parent it reads, the builder it fills,
/// and what each parent expression became in the builder's table.
struct RegionCopy<'p> {
    parent:   &'p Graph,
    builder:  GraphBuilder,
    imported: HashMap<ExprId, ExprId>,
}

impl RegionCopy<'_> {
    fn import(&mut self, id: ExprId) -> ExprId {
        self.builder
            .exprs()
            .import(&self.parent.exprs, id, &mut self.imported)
    }

    /// Fill the copy `id` of `source`, keeping the arms to `copies`.
    fn node(&mut self, id: NodeId, source: &Node, copies: &HashMap<NodeId, NodeId>) {
        let config = placeholder::map_expr_ids(&source.step.config, &mut |raw| {
            u32::try_from(raw).map_or(raw, |id| u64::from(self.import(ExprId::new(id)).raw()))
        });
        let precondition = source.precondition.map(|expr| self.import(expr));
        let expand = source.expand.clone().map(|expansion| match expansion {
            Expansion::ForEach {
                items,
                target,
                max_parallel,
                fail_fast,
            } => Expansion::ForEach {
                items: self.import(items),
                target,
                max_parallel,
                fail_fast,
            },
        });
        let mut groups = Vec::new();
        for group in &source.routing.groups {
            let mut arms = Vec::new();
            for arm in &group.arms {
                let Some(to) = copies.get(&arm.to).copied() else {
                    continue;
                };
                let mut edge = Edge::always(self.builder.next_edge_id(), to);
                if let Guard::Expr(guard) = arm.guard {
                    edge.guard = Guard::Expr(self.import(guard));
                }
                edge.map = arm.map.map(|map| self.import(map));
                edge.back = arm.back;
                edge.weight = arm.weight;
                edge.label.clone_from(&arm.label);
                edge.transition = arm.transition;
                arms.push(edge);
            }
            if !arms.is_empty() {
                groups.push(RoutingGroup::new(arms));
            }
        }
        let node = self.builder.node_mut(id);
        node.step = StepRef::new(source.step.kind.clone(), config);
        node.join = source.join;
        node.precondition = precondition;
        node.routing = Routing::groups(groups);
        node.budget = source.budget;
        node.retry = source.retry.clone();
        node.run_on_cancel = source.run_on_cancel;
        node.tolerates_failure = source.tolerates_failure;
        node.splice_policy = source.splice_policy;
        node.meta = source.meta.clone();
        node.expand = expand;
    }
}

impl<S> GraphBuilder<S> {
    fn with_default_scope() -> Self {
        let mut builder = Self::default();
        builder.add_scope(Scope::new(ScopeId::new(0)));
        builder
    }

    pub fn exprs(&mut self) -> &mut ExprTable<S> {
        &mut self.graph.body.exprs
    }

    pub fn graph(&self) -> &Graph<S> {
        &self.graph
    }

    pub fn graph_mut(&mut self) -> &mut Graph<S> {
        &mut self.graph
    }

    pub fn add_scope(&mut self, scope: Scope<S>) -> ScopeId<S> {
        let id = ScopeId::new(
            u32::try_from(self.graph.scopes.len()).expect("a graph never exceeds u32::MAX scopes"),
        );
        let mut scope = scope;
        scope.id = id;
        self.graph.body.scopes.push(scope);
        id
    }

    /// Add a node with terminal routing; wire it up afterwards.
    pub fn add_node(&mut self, name: &str, scope: ScopeId<S>, step: StepRef) -> NodeId<S> {
        let id = NodeId::new(
            u32::try_from(self.graph.nodes.len()).expect("a graph never exceeds u32::MAX nodes"),
        );
        self.graph.body.nodes.push(Node::new(id, name, scope, step));
        id
    }

    /// Add a node whose step is a bare kind with no config.
    pub fn add_step(
        &mut self,
        name: &str,
        scope: ScopeId<S>,
        kind: impl Into<StepKindId>,
    ) -> NodeId<S> {
        self.add_node(name, scope, StepRef::new(kind, Value::Null))
    }

    /// # Panics
    ///
    /// Panics when `id` did not come from this builder: the builder only hands
    /// out ids for nodes it holds.
    pub fn node_mut(&mut self, id: NodeId<S>) -> &mut Node<S> {
        self.graph
            .body
            .node_mut(id)
            .expect("builder handed out a valid node id")
    }

    pub fn set_join(&mut self, node: NodeId<S>, join: JoinPolicy) {
        self.node_mut(node).join = join;
    }

    pub fn set_budget(&mut self, node: NodeId<S>, budget: Budget) {
        self.node_mut(node).budget = budget;
    }

    pub fn set_precondition(&mut self, node: NodeId<S>, expr: ExprId<S>) {
        self.node_mut(node).precondition = Some(expr);
    }

    pub fn set_expansion(&mut self, node: NodeId<S>, expand: Expansion<S>) {
        self.node_mut(node).expand = Some(expand);
    }

    /// Attach frontend metadata to a node. Opaque to the engine.
    pub fn set_meta(&mut self, node: NodeId<S>, meta: Value) {
        self.node_mut(node).meta = meta;
    }

    pub fn mark_entry(&mut self, node: NodeId<S>) {
        self.graph.body.entry.push(node);
    }

    pub fn next_edge_id(&mut self) -> EdgeId<S> {
        let id = EdgeId::new(self.next_edge);
        self.next_edge += 1;
        id
    }

    /// Sequential `next:` — one group, one unconditional arm.
    pub fn link(&mut self, from: NodeId<S>, to: NodeId<S>) -> EdgeId<S> {
        let id = self.next_edge_id();
        self.node_mut(from).routing = Routing::next(Edge::always(id, to));
        id
    }

    /// One group of ordered arms: exactly one successor is chosen.
    pub fn select(&mut self, from: NodeId<S>, arms: Vec<Arm<S>>) -> Vec<EdgeId<S>> {
        self.select_with(from, arms, Fallthrough::NoEmit)
    }

    pub fn select_with(
        &mut self,
        from: NodeId<S>,
        arms: Vec<Arm<S>>,
        fallthrough: Fallthrough,
    ) -> Vec<EdgeId<S>> {
        let (edges, ids) = self.build_arms(arms);
        self.node_mut(from).routing = Routing::groups(vec![SelectGroup {
            policy: SelectionPolicy::FirstMatch,
            arms: edges,
            fallthrough,
        }]);
        ids
    }

    /// Explicit AND-split: one group per target, all emitting concurrently.
    pub fn fan_out(&mut self, from: NodeId<S>, targets: &[NodeId<S>]) -> Vec<EdgeId<S>> {
        let arms = targets.iter().map(|t| Arm::always(*t)).collect();
        let (edges, ids) = self.build_arms(arms);
        self.node_mut(from).routing = Routing::fan_out(edges);
        ids
    }

    /// Fan-out where each group carries its own guard: a group with no matching
    /// arm emits nothing, which is the OR-split.
    pub fn fan_out_groups(
        &mut self,
        from: NodeId<S>,
        groups: Vec<Vec<Arm<S>>>,
    ) -> Vec<Vec<EdgeId<S>>> {
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

    fn build_arms(&mut self, arms: Vec<Arm<S>>) -> (Vec<Edge<S>>, Vec<EdgeId<S>>) {
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

    /// Finish. Entry defaults to every node with no incoming edges when none
    /// was marked, which is what a linear frontend wants.
    pub fn build(mut self) -> Graph<S> {
        self.finish_entries();
        self.graph
    }

    fn finish_entries(&mut self) {
        if self.graph.entry.is_empty() {
            let entries: Vec<NodeId<S>> = self
                .graph
                .nodes
                .iter()
                .map(|n| n.id)
                .filter(|id| self.graph.in_degree(*id) == 0)
                .collect();
            self.graph.body.entry = entries;
        }
    }
}
