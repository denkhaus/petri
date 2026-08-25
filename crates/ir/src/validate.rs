//! Load-time validation: the invariants of §7, plus the structural checks the rest
//! of the system assumes (ids in range, node index == `NodeId`, ...).
//!
//! Every check runs, so one call reports every problem rather than the first.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use serde_json::Value;
use smol_str::SmolStr;

use crate::expr::Expr;
use crate::graph::{ExpandTarget, Expansion, ExprOrValue, Graph, Guard};
use crate::ids::{EdgeId, ExprId, NodeId, ScopeId};
use crate::step::StepRegistry;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    // ── Structure ──────────────────────────────────────────────────────────
    #[error("node at index {index} declares id {declared:?}; node ids must equal their index")]
    NodeIdMismatch { index: usize, declared: NodeId },
    #[error("scope at index {index} declares id {declared:?}; scope ids must equal their index")]
    ScopeIdMismatch { index: usize, declared: ScopeId },
    #[error("node {node:?} refers to unknown scope {scope:?}")]
    UnknownScope { node: NodeId, scope: ScopeId },
    #[error("edge {edge:?} on node {from:?} points at unknown node {to:?}")]
    UnknownTarget {
        from: NodeId,
        edge: EdgeId,
        to: NodeId,
    },
    #[error("node {node:?} uses step kind {kind} which is not registered")]
    UnknownStepKind { node: NodeId, kind: u32 },
    #[error("node {node:?} has an invalid step config: {message}")]
    BadStepConfig { node: NodeId, message: String },
    #[error("the graph has no entry nodes")]
    NoEntry,
    #[error("entry list refers to unknown node {0:?}")]
    UnknownEntry(NodeId),
    #[error("entry node {0:?} is listed twice")]
    DuplicateEntry(NodeId),
    #[error(
        "entry node {0:?} has an incoming forward edge; entry nodes are seeded, and only \
         a back edge may point at one"
    )]
    EntryHasIncoming(NodeId),

    // ── Invariant 1 ────────────────────────────────────────────────────────
    #[error("cycle through {0:?} contains no back edge")]
    CycleWithoutBackEdge(Vec<NodeId>),

    // ── Invariant 2 ────────────────────────────────────────────────────────
    #[error("node {node:?}: Guard::Always on arm {arm} is not the final arm of its group")]
    AlwaysNotLast { node: NodeId, arm: usize },

    // ── Invariant 3 ────────────────────────────────────────────────────────
    #[error("node {node:?}: select group {group} has no arms")]
    EmptyGroup { node: NodeId, group: usize },

    // ── Invariant 4 ────────────────────────────────────────────────────────
    #[error("node {0:?}: Budget.max_firings must be >= 1")]
    ZeroBudget(NodeId),
    #[error("node {0:?} is reachable through a back edge, so it needs a finite firing budget")]
    UnboundedLoopBudget(NodeId),

    // ── Invariant 5 ────────────────────────────────────────────────────────
    #[error("edge id {0:?} is used more than once")]
    DuplicateEdgeId(EdgeId),
    #[error("edge id {0:?} is reserved for seed tokens")]
    ReservedEdgeId(EdgeId),

    // ── Invariant 6 ────────────────────────────────────────────────────────
    #[error("{site} refers to expression {expr:?}, which is not in the table")]
    UnknownExpr { site: SmolStr, expr: ExprId },
    #[error("node {0:?} still carries an `expand`; executable plans are fully lowered")]
    HirFieldInPlan(NodeId),
    #[error("node {0:?} still carries an unresolved config placeholder")]
    HirConfigInPlan(NodeId),

    // ── Invariant 7 ────────────────────────────────────────────────────────
    #[error("node {node:?}: expansion subgraph entry {entry:?} does not reach exit {exit:?}")]
    ExitUnreachable {
        node: NodeId,
        entry: NodeId,
        exit: NodeId,
    },
    #[error(
        "node {node:?}: expansion subgraph exit {exit:?} does not postdominate entry {entry:?}; \
         {offender:?} can complete the region without reaching the exit"
    )]
    ExitNotPostdominator {
        node: NodeId,
        entry: NodeId,
        exit: NodeId,
        offender: NodeId,
    },
    #[error(
        "node {node:?}: edge {edge:?} crosses the expansion subgraph boundary; \
         only edges into the entry and out of the exit may cross"
    )]
    BoundaryCrossing { node: NodeId, edge: EdgeId },
}

/// The marker a HIR `StepRef.config` uses for a value that is still an expression.
/// Lowering replaces these with concrete values before execution.
pub const EXPR_PLACEHOLDER_KEY: &str = "$expr";

/// Validate a graph in HIR form: `expand` and config placeholders are allowed.
pub fn validate(graph: &Graph) -> Result<(), Vec<ValidationError>> {
    validate_with(graph, None)
}

/// Validate an executable plan: everything [`validate`] checks, plus invariant 6's
/// requirement that no HIR-only field survives.
pub fn validate_plan(graph: &Graph) -> Result<(), Vec<ValidationError>> {
    let mut errors = collect(graph, None);
    check_fully_lowered(graph, &mut errors);
    done(errors)
}

/// Validate a graph and resolve every step kind against `registry`.
pub fn validate_with(
    graph: &Graph,
    registry: Option<&StepRegistry>,
) -> Result<(), Vec<ValidationError>> {
    done(collect(graph, registry))
}

fn done(errors: Vec<ValidationError>) -> Result<(), Vec<ValidationError>> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn collect(graph: &Graph, registry: Option<&StepRegistry>) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    check_structure(graph, registry, &mut errors);
    check_edge_ids(graph, &mut errors);
    check_routing_shape(graph, &mut errors);
    check_exprs_resolve(graph, &mut errors);
    check_back_edges(graph, &mut errors);
    check_budgets(graph, &mut errors);
    check_expansions(graph, &mut errors);
    errors
}

// ── Structure ─────────────────────────────────────────────────────────────

fn check_structure(
    graph: &Graph,
    registry: Option<&StepRegistry>,
    errors: &mut Vec<ValidationError>,
) {
    for (index, scope) in graph.scopes.iter().enumerate() {
        if scope.id.index() != index {
            errors.push(ValidationError::ScopeIdMismatch {
                index,
                declared: scope.id,
            });
        }
    }

    for (index, node) in graph.nodes.iter().enumerate() {
        if node.id.index() != index {
            errors.push(ValidationError::NodeIdMismatch {
                index,
                declared: node.id,
            });
        }
        if graph.scope(node.scope).is_none() {
            errors.push(ValidationError::UnknownScope {
                node: node.id,
                scope: node.scope,
            });
        }
        if let Some(registry) = registry {
            match registry.get(node.step.kind) {
                None => errors.push(ValidationError::UnknownStepKind {
                    node: node.id,
                    kind: node.step.kind.raw(),
                }),
                Some(kind) => {
                    if let Err(message) = kind.validate_config(&node.step.config) {
                        errors.push(ValidationError::BadStepConfig {
                            node: node.id,
                            message,
                        });
                    }
                }
            }
        }
        for edge in node.routing.edges() {
            if graph.node(edge.to).is_none() {
                errors.push(ValidationError::UnknownTarget {
                    from: node.id,
                    edge: edge.id,
                    to: edge.to,
                });
            }
        }
    }

    if graph.entry.is_empty() {
        errors.push(ValidationError::NoEntry);
    }
    let mut seen = HashSet::new();
    for &entry in &graph.entry {
        if graph.node(entry).is_none() {
            errors.push(ValidationError::UnknownEntry(entry));
            continue;
        }
        if !seen.insert(entry) {
            errors.push(ValidationError::DuplicateEntry(entry));
        }
        // A loop head may be the entry: the seed starts generation 0, and the back
        // edge starts each later one. A forward edge into an entry is a
        // contradiction, though, because the entry is seeded rather than joined.
        if graph.edges().any(|edge| edge.to == entry && !edge.back) {
            errors.push(ValidationError::EntryHasIncoming(entry));
        }
    }
}

/// Invariant 5: edge ids are unique across the whole graph, and none reuses the
/// reserved seed id.
fn check_edge_ids(graph: &Graph, errors: &mut Vec<ValidationError>) {
    let mut seen = HashSet::new();
    let mut reported = HashSet::new();
    for edge in graph.edges() {
        if edge.id == EdgeId::SEED && reported.insert(edge.id) {
            errors.push(ValidationError::ReservedEdgeId(edge.id));
        }
        if !seen.insert(edge.id) && reported.insert(edge.id) {
            errors.push(ValidationError::DuplicateEdgeId(edge.id));
        }
    }
}

/// Invariants 2 and 3.
fn check_routing_shape(graph: &Graph, errors: &mut Vec<ValidationError>) {
    for node in &graph.nodes {
        for (group_index, group) in node.routing.groups.iter().enumerate() {
            if group.arms.is_empty() {
                errors.push(ValidationError::EmptyGroup {
                    node: node.id,
                    group: group_index,
                });
                continue;
            }
            let last = group.arms.len() - 1;
            for (arm_index, arm) in group.arms.iter().enumerate() {
                if arm.guard == Guard::Always && arm_index != last {
                    errors.push(ValidationError::AlwaysNotLast {
                        node: node.id,
                        arm: arm_index,
                    });
                }
            }
        }
    }
}

// ── Invariant 6 (references) ──────────────────────────────────────────────

fn check_exprs_resolve(graph: &Graph, errors: &mut Vec<ValidationError>) {
    let table = &graph.exprs;
    let check = |site: String, id: ExprId, errors: &mut Vec<ValidationError>| {
        if table.get(id).is_none() {
            errors.push(ValidationError::UnknownExpr {
                site: SmolStr::new(&site),
                expr: id,
            });
        }
    };

    for (id, expr) in table.iter() {
        let site = format!("expression {}", id.raw());
        for child in children(expr) {
            check(site.clone(), child, errors);
        }
    }

    for scope in &graph.scopes {
        for (key, value) in &scope.env {
            if let ExprOrValue::Expr(id) = value {
                check(format!("scope {} env `{key}`", scope.id), *id, errors);
            }
        }
    }

    for node in &graph.nodes {
        if let Some(pre) = node.precondition {
            check(format!("node {} precondition", node.id), pre, errors);
        }
        if let Some(Expansion::ForEach { items, .. }) = &node.expand {
            check(format!("node {} expansion items", node.id), *items, errors);
        }
        for edge in node.routing.edges() {
            if let Guard::Expr(id) = edge.guard {
                check(format!("edge {} guard", edge.id), id, errors);
            }
            if let Some(map) = edge.map {
                check(format!("edge {} map", edge.id), map, errors);
            }
        }
    }
}

fn children(expr: &Expr) -> Vec<ExprId> {
    match expr {
        Expr::Lit(_) | Expr::Var(_) => Vec::new(),
        Expr::Field(base, _) => vec![*base],
        Expr::Index(base, idx) => vec![*base, *idx],
        Expr::Unary(_, arg) => vec![*arg],
        Expr::Binary(_, lhs, rhs) => vec![*lhs, *rhs],
        Expr::Cond {
            cond,
            then,
            otherwise,
        } => vec![*cond, *then, *otherwise],
        Expr::Array(items) => items.clone(),
        Expr::Object(fields) => fields.iter().map(|(_, v)| *v).collect(),
        Expr::Call(_, args) => args.clone(),
    }
}

fn check_fully_lowered(graph: &Graph, errors: &mut Vec<ValidationError>) {
    for node in &graph.nodes {
        if node.expand.is_some() {
            errors.push(ValidationError::HirFieldInPlan(node.id));
        }
        if has_placeholder(&node.step.config) {
            errors.push(ValidationError::HirConfigInPlan(node.id));
        }
    }
}

fn has_placeholder(config: &Value) -> bool {
    match config {
        Value::Object(map) => {
            map.contains_key(EXPR_PLACEHOLDER_KEY) || map.values().any(has_placeholder)
        }
        Value::Array(items) => items.iter().any(has_placeholder),
        _ => false,
    }
}

// ── Invariant 1 ───────────────────────────────────────────────────────────

/// Every cycle contains at least one back edge — equivalently, the graph with back
/// edges removed is acyclic. Reports one representative cycle per offending
/// strongly connected component.
fn check_back_edges(graph: &Graph, errors: &mut Vec<ValidationError>) {
    // Iterative DFS over forward edges only, tracking the current path so a
    // rediscovered grey node yields the cycle itself, not just "a cycle exists".
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Grey,
        Black,
    }

    let n = graph.nodes.len();
    let mut color = vec![Color::White; n];
    let mut reported: HashSet<BTreeSet<u32>> = HashSet::new();

    let successors = |node: NodeId| -> Vec<NodeId> {
        graph
            .node(node)
            .into_iter()
            .flat_map(|nd| nd.routing.edges())
            .filter(|e| !e.back)
            .map(|e| e.to)
            .filter(|to| to.index() < n)
            .collect()
    };

    for start in (0..n).map(|i| NodeId::new(i as u32)) {
        if color[start.index()] != Color::White {
            continue;
        }
        // (node, index of the next successor to visit)
        let mut stack: Vec<(NodeId, usize)> = vec![(start, 0)];
        let mut path: Vec<NodeId> = vec![start];
        color[start.index()] = Color::Grey;

        while let Some((node, cursor)) = stack.pop() {
            let succ = successors(node);
            if cursor < succ.len() {
                stack.push((node, cursor + 1));
                let next = succ[cursor];
                match color[next.index()] {
                    Color::Grey => {
                        let at = path.iter().position(|p| *p == next).unwrap_or(0);
                        let cycle: Vec<NodeId> = path[at..].to_vec();
                        let key: BTreeSet<u32> = cycle.iter().map(|c| c.raw()).collect();
                        if reported.insert(key) {
                            errors.push(ValidationError::CycleWithoutBackEdge(cycle));
                        }
                    }
                    Color::White => {
                        color[next.index()] = Color::Grey;
                        path.push(next);
                        stack.push((next, 0));
                    }
                    Color::Black => {}
                }
            } else {
                color[node.index()] = Color::Black;
                path.pop();
            }
        }
    }
}

// ── Invariant 4 ───────────────────────────────────────────────────────────

fn check_budgets(graph: &Graph, errors: &mut Vec<ValidationError>) {
    for node in &graph.nodes {
        if node.budget.max_firings == 0 {
            errors.push(ValidationError::ZeroBudget(node.id));
        }
    }

    // Anything downstream of a back edge can fire once per generation, so its cap
    // is what makes the run terminate.
    let mut queue: VecDeque<NodeId> = graph
        .edges()
        .filter(|e| e.back)
        .map(|e| e.to)
        .filter(|to| graph.node(*to).is_some())
        .collect();
    let mut seen: HashSet<NodeId> = queue.iter().copied().collect();
    while let Some(node) = queue.pop_front() {
        if let Some(nd) = graph.node(node) {
            if !nd.budget.is_finite() {
                errors.push(ValidationError::UnboundedLoopBudget(node));
            }
            for edge in nd.routing.edges() {
                if graph.node(edge.to).is_some() && seen.insert(edge.to) {
                    queue.push_back(edge.to);
                }
            }
        }
    }
}

// ── Invariant 7 ───────────────────────────────────────────────────────────

fn check_expansions(graph: &Graph, errors: &mut Vec<ValidationError>) {
    for node in &graph.nodes {
        let Some(Expansion::ForEach {
            target: ExpandTarget::Subgraph { entry, exit },
            ..
        }) = &node.expand
        else {
            continue;
        };
        let (entry, exit) = (*entry, *exit);
        if graph.node(entry).is_none() || graph.node(exit).is_none() {
            errors.push(ValidationError::ExitUnreachable {
                node: node.id,
                entry,
                exit,
            });
            continue;
        }

        // The region: everything reachable from the entry without passing through
        // the exit. The exit belongs to it but is not traversed.
        let mut region: HashSet<NodeId> = HashSet::from([entry]);
        let mut queue = VecDeque::from([entry]);
        while let Some(node_id) = queue.pop_front() {
            if node_id == exit {
                continue;
            }
            let Some(nd) = graph.node(node_id) else {
                continue;
            };
            for edge in nd.routing.edges() {
                if graph.node(edge.to).is_some() && region.insert(edge.to) {
                    queue.push_back(edge.to);
                }
            }
        }

        if !region.contains(&exit) || entry == exit && graph.node(entry).is_none() {
            errors.push(ValidationError::ExitUnreachable {
                node: node.id,
                entry,
                exit,
            });
            continue;
        }

        // Postdominance: no node inside the region may finish the region without
        // reaching the exit, so every non-exit member must have somewhere to go.
        for &member in &region {
            if member == exit {
                continue;
            }
            let Some(nd) = graph.node(member) else {
                continue;
            };
            if nd.routing.groups.is_empty() {
                errors.push(ValidationError::ExitNotPostdominator {
                    node: node.id,
                    entry,
                    exit,
                    offender: member,
                });
            }
        }

        // Boundary: edges may only enter at the entry and leave from the exit.
        for source in &graph.nodes {
            let inside = region.contains(&source.id) && source.id != exit;
            for edge in source.routing.edges() {
                let target_inside = region.contains(&edge.to);
                let crosses_in = !inside && source.id != exit && target_inside && edge.to != entry;
                let crosses_out = inside && !target_inside;
                if crosses_in || crosses_out {
                    errors.push(ValidationError::BoundaryCrossing {
                        node: node.id,
                        edge: edge.id,
                    });
                }
            }
        }
    }
}

/// Map of edge id to the node it leaves, built once for callers that need it often.
pub fn edge_sources(graph: &Graph) -> HashMap<EdgeId, NodeId> {
    let mut map = HashMap::new();
    for node in &graph.nodes {
        for edge in node.routing.edges() {
            map.insert(edge.id, node.id);
        }
    }
    map
}
