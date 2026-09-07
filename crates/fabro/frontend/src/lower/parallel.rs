//! Parallel nodes (`component`) and their fan-ins.
//!
//! Every branch of a parallel node runs as an internal child invocation, the
//! way Fabro runs it as a branch task: the branch target is lowered into a
//! graph of its own, and the parent graph's node for that target becomes a
//! `fabro/branch` step that starts the child with a snapshot of the parent's
//! context, waits for it, and returns the branch envelope. The child inherits
//! the parent's sandbox and workspace and keeps its own context; its changes
//! come back as output only and never merge into the parent's `kv`. A branch
//! never follows its target's outgoing edges: every branch routes to the
//! join, the one direct successor all branch targets share.
//!
//! Static branches are the parallel node's outgoing edges, in order, each
//! its own branch node (a duplicate target gets a synthetic node so each
//! branch has its own index). A `for_each` fan-out expands the one template
//! node over the runtime items with the same branch step; an empty list
//! expands to one placeholder item so the fan-in still fires and reports
//! zero branches.
//!
//! The fan-in the branches join at collects the envelopes in branch order,
//! publishes `parallel.results` and `parallel.branch_count`, and takes the
//! aggregate status. A join that is not a `tripleoctagon` gets a synthetic
//! fan-in in front of it, so `parallel.results` is published either way and
//! the join runs once.
//!
//! `max_parallel` is not an expansion limit: it bounds the *attempts* of a
//! fork's branches through the coordinator's attempt admission, so a branch
//! waiting out a retry backoff holds no slot. Fabro's normalization applies:
//! missing, non-integer or negative is 4, zero is 1.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use frontend::Span;
use ir::placeholder::{BRANCH_ROLE_META, EXPR_PLACEHOLDER_KEY};
use ir::{
    BinOp, Budget, Edge, ExpandTarget, Expr, ExprId, ExprOrValue, ExprTable, GraphBuilder,
    JoinPolicy, NodeId, ResultProjection, RetryPolicy, Routing, RoutingGroup, StepRef,
};
use serde_json::{Map, Value, json};
use smol_str::SmolStr;

use super::{Ctx, Kind, MAX_FOR_EACH_ITEMS, Resolved, placeholder};
use crate::kinds::{BRANCH_KIND, EMPTY_BRANCH_MARKER, FAN_IN_KIND, PROMPT_KIND};
use crate::model::{AttrValue, EdgeDecl, NodeDecl, Workflow};

/// Fabro's `max_parallel` when the attribute is missing, not an integer, or
/// negative.
pub const DEFAULT_MAX_PARALLEL: u32 = 4;

/// The `meta.kind` of a parent-side branch node: the delegate that starts
/// one branch's child invocation and returns its envelope.
pub const BRANCH_META_KIND: &str = "parallel.branch";

/// How a branch's position is known: fixed at lowering for a static branch,
/// bound by the expansion for a `for_each` item.
#[derive(Clone, Copy)]
enum BranchIndex {
    Static(u32),
    Item,
}

impl Ctx<'_> {
    /// Lower every parallel node and configure every fan-in. Nested parallel
    /// nodes (a parallel node that is another's branch target) lower first,
    /// so the outer branch's child graph copies the finished inner region.
    pub(super) fn parallel(
        &mut self,
        workflow: &Workflow,
        resolved: &[Resolved],
        exit: NodeId,
        goal_check: Option<NodeId>,
    ) {
        for (node, res) in workflow.nodes.iter().zip(resolved) {
            if res.kind == Kind::FanIn {
                self.fan_in_step(node, res.id);
            }
        }
        let mut pending: Vec<&NodeDecl> = workflow
            .nodes
            .iter()
            .filter(|node| self.kinds.get(&node.id) == Some(&Kind::Parallel))
            .collect();
        let mut done: HashSet<String> = HashSet::new();
        while !pending.is_empty() {
            let ready = pending.iter().position(|node| {
                workflow.outgoing(&node.id).iter().all(|edge| {
                    self.kinds.get(&edge.to) != Some(&Kind::Parallel) || done.contains(&edge.to)
                })
            });
            let Some(position) = ready else {
                for node in &pending {
                    self.diags.error(
                        "fabro.parallel.cycle",
                        node.span.clone(),
                        format!(
                            "parallel node `{}` is a branch of a parallel node that is a branch of \
                             it; parallel nodes cannot nest in a cycle",
                            node.id
                        ),
                    );
                }
                return;
            };
            let node = pending.remove(position);
            self.lower_parallel(node, workflow, exit, goal_check);
            done.insert(node.id.clone());
        }
    }

    /// The fan-in step: the ordered branch envelopes its inputs carry. A
    /// prompted fan-in (already a `fabro/prompt` step) reads the same list.
    fn fan_in_step(&mut self, node: &NodeDecl, id: NodeId) {
        let ordered = {
            let exprs = self.b.exprs();
            let inputs = exprs.var("inputs");
            let index = exprs.lit("index");
            let sorted = exprs.call("sort_by_key", vec![inputs, index]);
            let value = exprs.lit("value");
            exprs.call("pluck", vec![sorted, value])
        };
        let step = &mut self.b.node_mut(id).step;
        if step.kind == PROMPT_KIND {
            if let Value::Object(config) = &mut step.config {
                config.insert("branch_results".into(), placeholder(ordered));
            }
            return;
        }
        *step = StepRef::new(
            FAN_IN_KIND,
            json!({
                "label": node.attrs.text("label").unwrap_or_else(|| node.id.clone()),
                "node": node.id,
                "results": placeholder(ordered),
            }),
        );
    }

    fn lower_parallel(
        &mut self,
        node: &NodeDecl,
        workflow: &Workflow,
        exit: NodeId,
        goal_check: Option<NodeId>,
    ) {
        let edges = workflow.outgoing(&node.id);
        for edge in &edges {
            if edge
                .attrs
                .text("condition")
                .is_some_and(|c| !c.trim().is_empty())
            {
                self.diags.error(
                    "fabro.parallel.conditional_branch",
                    edge.span.clone(),
                    "a parallel node's branches are unconditional",
                );
            }
        }
        if edges.is_empty() {
            self.diags.error(
                "fabro.parallel.no_branches",
                node.span.clone(),
                format!("parallel node `{}` has no branches", node.id),
            );
            return;
        }
        let max_parallel = self.max_parallel(node);
        if let Some(source) = node.attrs.text("for_each") {
            self.dynamic_branches(node, &edges, &source, max_parallel, workflow, exit, goal_check);
        } else {
            self.static_branches(node, &edges, max_parallel, workflow, exit, goal_check);
        }
    }

    /// Fabro's `max_parallel`: missing, non-integer or negative is 4; zero
    /// is 1. Petri says so where Fabro is silent.
    fn max_parallel(&mut self, node: &NodeDecl) -> u32 {
        let Some(attr) = node.attrs.get("max_parallel") else {
            return DEFAULT_MAX_PARALLEL;
        };
        let parsed = match &attr.value {
            AttrValue::Int(n) => Some(*n),
            AttrValue::Str(s) => s.trim().parse::<i64>().ok(),
            AttrValue::Float(_) | AttrValue::Bool(_) => None,
        };
        let span = attr.span.clone();
        match parsed {
            Some(n) if n > 0 => u32::try_from(n).unwrap_or(u32::MAX),
            Some(0) => {
                self.diags.warning(
                    "fabro.max_parallel.normalized",
                    span,
                    format!(
                        "`max_parallel=0` on `{}` runs one branch at a time, as Fabro does",
                        node.id
                    ),
                );
                1
            }
            Some(n) => {
                self.diags.warning(
                    "fabro.max_parallel.normalized",
                    span,
                    format!(
                        "`max_parallel={n}` on `{}` is negative; Fabro reads it as \
                         {DEFAULT_MAX_PARALLEL}",
                        node.id
                    ),
                );
                DEFAULT_MAX_PARALLEL
            }
            None => {
                self.diags.warning(
                    "fabro.max_parallel.normalized",
                    span,
                    format!(
                        "`max_parallel={}` on `{}` is not an integer; Fabro reads it as \
                         {DEFAULT_MAX_PARALLEL}",
                        attr.value.as_text(),
                        node.id
                    ),
                );
                DEFAULT_MAX_PARALLEL
            }
        }
    }

    /// The node every branch joins at: the one direct successor all branch
    /// targets share (the lowest id when there are several, as Fabro picks
    /// it). A `tripleoctagon` join is the collector itself; any other join
    /// gets a synthetic fan-in in front of it, so the results are published
    /// and the join runs once. Edges a branch target has to other nodes are
    /// never taken and are reported.
    fn branch_collector(
        &mut self,
        fork: &NodeDecl,
        branches: &[&EdgeDecl],
        workflow: &Workflow,
        exit: NodeId,
        goal_check: Option<NodeId>,
    ) -> Option<NodeId> {
        let mut common: Option<HashSet<String>> = None;
        for branch in branches {
            let targets: HashSet<String> = workflow
                .outgoing(&branch.to)
                .into_iter()
                .map(|edge| edge.to.clone())
                .collect();
            common = Some(match common {
                None => targets,
                Some(shared) => shared.intersection(&targets).cloned().collect(),
            });
        }
        let mut shared: Vec<String> = common.unwrap_or_default().into_iter().collect();
        shared.sort();
        let Some(join) = shared.into_iter().next() else {
            self.diags.error(
                "fabro.parallel.no_join",
                fork.span.clone(),
                format!(
                    "the branches of parallel node `{}` share no direct successor to join at; \
                     every branch target must have an edge to the same node",
                    fork.id
                ),
            );
            return None;
        };
        for branch in branches {
            for edge in workflow.outgoing(&branch.to) {
                if edge.to != join {
                    self.diags.warning(
                        "fabro.parallel.branch_edge_ignored",
                        edge.span.clone(),
                        format!(
                            "`{} -> {}` is never taken: `{}` runs as a branch of `{}` and returns \
                             to the join `{join}`",
                            edge.from, edge.to, edge.from, fork.id
                        ),
                    );
                }
            }
        }
        let join_id = self.ids.get(&join).copied()?;
        if self.kinds.get(&join) == Some(&Kind::FanIn) {
            return Some(join_id);
        }
        let target = goal_check.filter(|_| join_id == exit).unwrap_or(join_id);
        let name = format!("{}.fan_in", fork.id);
        let results = placeholder(self.ordered_results());
        let collector = self.b.add_node(
            &name,
            self.scope,
            StepRef::new(
                FAN_IN_KIND,
                json!({
                    "label": format!("Fan-in of {}", fork.id),
                    "node": name,
                    "results": results,
                }),
            ),
        );
        self.spans.insert(collector, fork.span.clone());
        self.b.set_meta(
            collector,
            json!({
                "label": format!("Fan-in of {}", fork.id),
                "shape": "tripleoctagon",
                "kind": Kind::FanIn.name(),
                "classes": [],
                "synthetic": true,
                "span": { "line": fork.span.line, "column": fork.span.column },
            }),
        );
        self.b.set_join(collector, JoinPolicy::All);
        self.b.link(collector, target);
        Some(collector)
    }

    fn ordered_results(&mut self) -> ExprId {
        let exprs = self.b.exprs();
        let inputs = exprs.var("inputs");
        let index = exprs.lit("index");
        let sorted = exprs.call("sort_by_key", vec![inputs, index]);
        let value = exprs.lit("value");
        exprs.call("pluck", vec![sorted, value])
    }

    fn static_branches(
        &mut self,
        fork: &NodeDecl,
        edges: &[&EdgeDecl],
        max_parallel: u32,
        workflow: &Workflow,
        exit: NodeId,
        goal_check: Option<NodeId>,
    ) {
        let mut ok = true;
        for edge in edges {
            let kind = self.kinds.get(&edge.to).copied();
            if matches!(kind, None | Some(Kind::Start | Kind::Exit | Kind::FanIn)) {
                self.diags.error(
                    "fabro.parallel.bad_branch_target",
                    edge.to_span.clone(),
                    format!(
                        "`{}` cannot be a branch of parallel node `{}`; a branch target is a \
                         stage that does work and then returns to the join",
                        edge.to, fork.id
                    ),
                );
                ok = false;
            }
        }
        if !ok {
            return;
        }
        let Some(collector) = self.branch_collector(fork, edges, workflow, exit, goal_check)
        else {
            return;
        };
        let fork_id = self.ids[&fork.id];
        let mut seen: HashSet<&str> = HashSet::new();
        let mut branch_nodes = Vec::with_capacity(edges.len());
        for (index, edge) in edges.iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let target = self.ids[&edge.to];
            let kind = self.kinds[&edge.to];
            let region = self.branch_region(&edge.to, target, kind);
            let Some(child) = self.branch_child(&edge.to, &region, Some((fork_id, index))) else {
                continue;
            };
            let branch_node = if seen.insert(edge.to.as_str()) {
                target
            } else {
                // A duplicate target: its own branch node, so the index and
                // the result stay its own.
                let duplicate = self.b.add_node(
                    &format!("{}.branch{index}", edge.to),
                    self.scope,
                    StepRef::new("noop", Value::Null),
                );
                let meta = self.b.graph().node(target).map(|n| n.meta.clone());
                if let Some(meta) = meta {
                    self.b.set_meta(duplicate, meta);
                }
                self.spans.insert(duplicate, edge.to_span.clone());
                duplicate
            };
            self.branch_step(
                branch_node,
                fork,
                &edge.to,
                BranchIndex::Static(index),
                max_parallel,
                &child,
                collector,
                kind,
            );
            branch_nodes.push(branch_node);
        }
        self.b.fan_out(fork_id, &branch_nodes);
    }

    fn dynamic_branches(
        &mut self,
        fork: &NodeDecl,
        edges: &[&EdgeDecl],
        source: &str,
        max_parallel: u32,
        workflow: &Workflow,
        exit: NodeId,
        goal_check: Option<NodeId>,
    ) {
        let span = fork.attrs.span_of("for_each", &fork.span);
        let key = source
            .strip_prefix("context.")
            .unwrap_or(source)
            .trim()
            .to_string();
        if key.is_empty() {
            self.diags.error(
                "fabro.for_each.source",
                span,
                format!("`for_each` on `{}` must name a context key", fork.id),
            );
            return;
        }
        if edges.len() != 1 {
            self.diags.error(
                "fabro.for_each.template_edges",
                fork.span.clone(),
                format!(
                    "`for_each` node `{}` needs exactly one template edge",
                    fork.id
                ),
            );
            return;
        }
        let template = edges[0];
        let Some(target) = self.ids.get(&template.to).copied() else {
            return;
        };
        let kind = self.kinds[&template.to];
        if !kind.is_llm() {
            self.diags.error(
                "fabro.for_each.target",
                template.to_span.clone(),
                format!(
                    "the `for_each` template `{}` must be an agent or prompt node",
                    template.to
                ),
            );
            return;
        }
        if workflow
            .node(&template.to)
            .is_some_and(|n| n.attrs.contains("for_each"))
        {
            self.diags.error(
                "fabro.for_each.nested",
                template.to_span.clone(),
                "nested `for_each` is not supported",
            );
            return;
        }
        let Some(collector) = self.branch_collector(fork, edges, workflow, exit, goal_check)
        else {
            return;
        };
        let Some(child) = self.branch_child(&template.to, &[target], None) else {
            return;
        };
        let fork_id = self.ids[&fork.id];

        // The parallel node evaluates the item array and hands it on. An
        // empty array becomes one placeholder item, so the template still
        // fires once (starting no child) and the fan-in still joins.
        let items = {
            let exprs = self.b.exprs();
            let kv = exprs.var("kv");
            let name = exprs.lit(key.as_str());
            let items = exprs.call("get", vec![kv, name]);
            let null = exprs.lit(Value::Null);
            let present = exprs.binary(BinOp::Ne, items, null);
            let len = exprs.call("len", vec![items]);
            let zero = exprs.lit(0);
            let empty = exprs.binary(BinOp::Eq, len, zero);
            let both = exprs.binary(BinOp::And, present, empty);
            let marker = exprs.lit(json!({ EMPTY_BRANCH_MARKER: true }));
            let placeholder_list = exprs.array(vec![marker]);
            exprs.cond(both, placeholder_list, items)
        };
        self.b.node_mut(fork_id).step = StepRef::new("noop", placeholder(items));
        let cap = {
            let exprs = self.b.exprs();
            let kv = exprs.var("kv");
            let name = exprs.lit(key.as_str());
            let items = exprs.call("get", vec![kv, name]);
            let len = exprs.call("len", vec![items]);
            let limit = exprs.lit(MAX_FOR_EACH_ITEMS);
            exprs.binary(BinOp::Le, len, limit)
        };
        self.b.set_precondition(fork_id, cap);
        self.b.link(fork_id, target);
        self.branch_step(
            target,
            fork,
            &template.to,
            BranchIndex::Item,
            max_parallel,
            &child,
            collector,
            kind,
        );
        let input = self.b.exprs().var("input");
        ir::parallel_for_each(&mut self.b, target, input, ExpandTarget::Node, None, false);
    }

    /// The parent-graph nodes a branch child copies: the target alone, or a
    /// nested parallel node with its branch nodes and its collector.
    fn branch_region(&self, target_name: &str, target: NodeId, kind: Kind) -> Vec<NodeId> {
        if kind != Kind::Parallel {
            return vec![target];
        }
        let graph = self.b.graph();
        let mut region = vec![target];
        let mut collector = None;
        if let Some(node) = graph.node(target) {
            for arm in node.routing.edges() {
                region.push(arm.to);
                if let Some(branch) = graph.node(arm.to) {
                    collector = branch.routing.edges().next().map(|edge| edge.to);
                }
            }
        }
        // A nested parallel node that failed its own lowering has no
        // collector; its diagnostic already stands, and the region is what
        // exists.
        let _ = target_name;
        region.extend(collector);
        region
    }

    /// Turn the parent node `branch_node` into the branch step for `target`.
    #[expect(
        clippy::too_many_arguments,
        reason = "one branch is described by exactly these facts"
    )]
    fn branch_step(
        &mut self,
        branch_node: NodeId,
        fork: &NodeDecl,
        target: &str,
        index: BranchIndex,
        max_parallel: u32,
        child: &str,
        collector: NodeId,
        kind: Kind,
    ) {
        let label = self
            .b
            .graph()
            .node(branch_node)
            .and_then(|n| n.meta.get("label").and_then(Value::as_str))
            .map_or_else(|| target.to_string(), str::to_string);
        let mut config = Map::new();
        config.insert("label".into(), Value::String(label));
        config.insert("node".into(), Value::String(target.to_string()));
        config.insert("fork".into(), Value::String(fork.id.clone()));
        let index_expr = match index {
            BranchIndex::Static(i) => {
                config.insert("index".into(), Value::from(i));
                self.b.exprs().lit(u64::from(i))
            }
            BranchIndex::Item => {
                let item = self.b.exprs().var("item");
                config.insert("item".into(), placeholder(item));
                config.insert("for_each".into(), Value::Bool(true));
                let index = self.b.exprs().var("index");
                config.insert("index".into(), placeholder(index));
                index
            }
        };
        config.insert("max_parallel".into(), Value::from(max_parallel));
        config.insert("child_digest".into(), Value::String(child.to_string()));
        config.insert("target_kind".into(), Value::String(kind.name().into()));
        let kv = self.b.exprs().var("kv");
        config.insert("kv".into(), placeholder(kv));
        let generation = self.b.exprs().var("generation");
        config.insert("generation".into(), placeholder(generation));
        if kind.is_llm() {
            let nodes = self.b.exprs().var("nodes");
            config.insert("nodes".into(), placeholder(nodes));
        }
        let payload = {
            let exprs = self.b.exprs();
            let output = exprs.var("output");
            exprs.object(vec![("index", index_expr), ("value", output)])
        };
        let edge_id = self.b.next_edge_id();
        let mut edge = Edge::always(edge_id, collector);
        edge.map = Some(payload);
        let node = self.b.node_mut(branch_node);
        node.step = StepRef::new(BRANCH_KIND, Value::Object(config));
        node.retry = RetryPolicy::none();
        node.budget = Budget::new(1, Duration::ZERO);
        node.precondition = None;
        node.routing = Routing::next(edge);
        if let Value::Object(meta) = &mut node.meta {
            // The parent-side node stands for the branch, not the stage: the
            // stage runs in the child with the target's own metadata. Hosts
            // tell the two apart by kind, and skip this one as a lowering
            // artifact where they skip `goal_check`.
            let mut branch = json!({ "fork": fork.id, "target": target });
            if let BranchIndex::Static(i) = index {
                branch["index"] = Value::from(i);
            }
            meta.insert("branch".into(), branch);
            meta.insert("kind".into(), Value::String(BRANCH_META_KIND.into()));
            meta.insert("synthetic".into(), Value::Bool(true));
        }
    }

    /// The child graph a branch runs: a copy of `region` from the parent
    /// graph with its own expression table, the same scopes, the same run
    /// parameters, the entry at the region's first node and the result at
    /// its last. The copy drops every edge that leaves the region (a branch
    /// never follows its target's edges) and the target's explicit routes (a
    /// branch has none, so its failure policy applies unconditionally).
    /// Returns the registered digest.
    fn branch_child(
        &mut self,
        target_name: &str,
        region: &[NodeId],
        role: Option<(NodeId, u32)>,
    ) -> Option<String> {
        let mut builder = GraphBuilder::bare();
        let mut copier = ExprCopier::default();
        let parent = self.b.graph();
        for scope in &parent.scopes {
            let mut copy = scope.clone();
            for value in copy.env.values_mut() {
                if let ExprOrValue::Expr(id) = value {
                    *id = copier.copy(&parent.exprs, *id, builder.exprs());
                }
            }
            builder.add_scope(copy);
        }
        let mut remap: HashMap<NodeId, NodeId> = HashMap::new();
        for old in region {
            let Some(node) = parent.node(*old) else {
                continue;
            };
            let id = builder.add_node(&node.name, node.scope, StepRef::new("noop", Value::Null));
            remap.insert(*old, id);
        }
        for (position, old) in region.iter().enumerate() {
            let Some(source) = parent.node(*old) else {
                continue;
            };
            let id = remap[old];
            let mut config = copier.copy_config(&parent.exprs, &source.step.config, builder.exprs());
            if position == 0
                && let Value::Object(map) = &mut config
            {
                map.remove(super::ROUTES_KEY);
                if source.step.kind == crate::kinds::AGENT_KIND || source.step.kind == PROMPT_KIND {
                    // The child's own records are empty when its target
                    // starts: the preamble renders from the parent's records
                    // at fork time, which the branch step puts in the
                    // snapshot.
                    let nodes = context_read(builder.exprs(), crate::kinds::BRANCH_NODES_KEY);
                    map.insert("nodes".into(), placeholder(nodes));
                    let item = context_read(builder.exprs(), crate::kinds::BRANCH_ITEM_KEY);
                    map.insert("item_data".into(), placeholder(item));
                    // Fabro's branch rules: threads are inert and an explicit
                    // `full` degrades to `summary:high`.
                    map.insert("branch".into(), Value::Bool(true));
                }
            }
            let precondition = source
                .precondition
                .map(|expr| copier.copy(&parent.exprs, expr, builder.exprs()));
            let expand = source.expand.clone().map(|expansion| match expansion {
                ir::Expansion::ForEach {
                    items,
                    target,
                    max_parallel,
                    fail_fast,
                } => ir::Expansion::ForEach {
                    items: copier.copy(&parent.exprs, items, builder.exprs()),
                    target,
                    max_parallel,
                    fail_fast,
                },
            });
            let mut groups = Vec::new();
            for group in &source.routing.groups {
                let mut arms = Vec::new();
                for arm in &group.arms {
                    let Some(to) = remap.get(&arm.to).copied() else {
                        continue;
                    };
                    let mut edge = Edge::always(builder.next_edge_id(), to);
                    if let ir::Guard::Expr(guard) = arm.guard {
                        edge.guard = ir::Guard::Expr(copier.copy(&parent.exprs, guard, builder.exprs()));
                    }
                    edge.map = arm
                        .map
                        .map(|map| copier.copy(&parent.exprs, map, builder.exprs()));
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
            let mut meta = source.meta.clone();
            if position == 0
                && let Some((fork, index)) = role
                && let Value::Object(map) = &mut meta
            {
                map.insert(
                    BRANCH_ROLE_META.into(),
                    json!({ "fork": fork.raw(), "index": index }),
                );
            }
            let node = builder.node_mut(id);
            node.step = StepRef::new(source.step.kind.clone(), config);
            node.join = source.join;
            node.precondition = precondition;
            node.routing = Routing::groups(groups);
            node.budget = source.budget.clone();
            node.retry = source.retry.clone();
            node.run_on_cancel = source.run_on_cancel;
            node.tolerates_failure = source.tolerates_failure;
            node.splice_policy = source.splice_policy;
            node.meta = meta;
            node.expand = expand;
        }
        let entry = remap.get(region.first()?).copied()?;
        let result = remap.get(region.last()?).copied()?;
        builder.mark_entry(entry);
        let mut graph = builder.build();
        graph.result = ResultProjection::NodeOutput(result);
        graph.params = self.params();
        let report = ir::check(&graph);
        let span = self
            .ids
            .get(target_name)
            .and_then(|id| self.spans.get(id).cloned())
            .unwrap_or_else(|| Span::file(target_name));
        for error in &report.errors {
            let mut d = frontend::Diagnostic::error(
                error.code(),
                span.clone(),
                format!("branch `{target_name}`: {error}"),
            );
            if let Some(hint) = error.hint() {
                d = d.with_hint(hint);
            }
            self.diags.push(d);
        }
        if !report.errors.is_empty() {
            return None;
        }
        let digest = frontend::graph_digest(&graph);
        self.children.push(graph);
        Some(digest)
    }
}

/// `get(kv, key)` in `table`.
fn context_read(table: &mut ExprTable, key: &str) -> ExprId {
    let kv = table.var("kv");
    let name = table.lit(key);
    table.call("get", vec![kv, name])
}

/// Copies expressions from one table into another, once each.
#[derive(Default)]
struct ExprCopier {
    memo: BTreeMap<u32, ExprId>,
}

impl ExprCopier {
    fn copy(&mut self, from: &ExprTable, id: ExprId, to: &mut ExprTable) -> ExprId {
        if let Some(copied) = self.memo.get(&id.raw()) {
            return *copied;
        }
        let expr = from
            .get(id)
            .cloned()
            .expect("a lowered expression id names an entry in its own table");
        let copied = match expr {
            Expr::Lit(value) => to.lit(value),
            Expr::Var(name) => to.var(&name),
            Expr::Field(base, name) => {
                let base = self.copy(from, base, to);
                to.field(base, &name)
            }
            Expr::Index(base, index) => {
                let base = self.copy(from, base, to);
                let index = self.copy(from, index, to);
                to.index(base, index)
            }
            Expr::Unary(op, arg) => {
                let arg = self.copy(from, arg, to);
                to.unary(op, arg)
            }
            Expr::Binary(op, lhs, rhs) => {
                let lhs = self.copy(from, lhs, to);
                let rhs = self.copy(from, rhs, to);
                to.binary(op, lhs, rhs)
            }
            Expr::Cond {
                cond,
                then,
                otherwise,
            } => {
                let cond = self.copy(from, cond, to);
                let then = self.copy(from, then, to);
                let otherwise = self.copy(from, otherwise, to);
                to.cond(cond, then, otherwise)
            }
            Expr::Array(items) => {
                let items = items
                    .into_iter()
                    .map(|item| self.copy(from, item, to))
                    .collect();
                to.array(items)
            }
            Expr::Object(fields) => {
                let fields: Vec<(SmolStr, ExprId)> = fields
                    .into_iter()
                    .map(|(key, value)| (key, self.copy(from, value, to)))
                    .collect();
                to.object(
                    fields
                        .iter()
                        .map(|(key, value)| (key.as_str(), *value))
                        .collect(),
                )
            }
            Expr::Call(name, args) => {
                let args = args
                    .into_iter()
                    .map(|arg| self.copy(from, arg, to))
                    .collect();
                to.call(&name, args)
            }
        };
        self.memo.insert(id.raw(), copied);
        copied
    }

    /// A step config with every `{"$expr": id}` placeholder pointing into
    /// `to`.
    fn copy_config(&mut self, from: &ExprTable, config: &Value, to: &mut ExprTable) -> Value {
        match config {
            Value::Object(map) => {
                if map.len() == 1
                    && let Some(id) = map.get(EXPR_PLACEHOLDER_KEY).and_then(Value::as_u64)
                    && let Ok(id) = u32::try_from(id)
                {
                    return placeholder(self.copy(from, ExprId::new(id), to));
                }
                Value::Object(
                    map.iter()
                        .map(|(key, value)| (key.clone(), self.copy_config(from, value, to)))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.copy_config(from, item, to))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}
