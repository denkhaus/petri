//! The state machine itself.
//!
//! `apply` is deterministic: no IO, no clocks, no randomness. Timeouts and step
//! results arrive as events. Routing emits tokens by feeding events back to itself
//! through an internal queue, and every one of those is logged before it is applied,
//! so the log is a complete, replayable account of the run.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ir::{
    CancelScopeId, Control, ExpandTarget, Expansion, FiringId, Generation, Guard, JoinPolicy, Node,
    NodeId, Outcome, Status, Token, Value, eval, eval_bool,
};
use smol_str::SmolStr;

use crate::context::{clone_bindings, firing_context, outcome_context, resolve_config};
use crate::event::{Command, Event, SpliceClone, SubgraphSplice};
use crate::state::{EngineState, Firing, FiringRecord, RunError, Splice, synthetic};

/// Apply one event and return the commands it produced.
///
/// The event may cause the core to emit further events of its own (tokens, splices,
/// cascading cancellations). Those are drained inside this call, so the caller only
/// ever feeds in events that came from outside.
pub fn apply(mut state: EngineState, ev: Event) -> (EngineState, Vec<Command>) {
    let mut commands = Vec::new();
    let mut queue: VecDeque<Event> = VecDeque::from([ev]);

    while let Some(event) = queue.pop_front() {
        state.log.append(event.clone());
        step(&mut state, event, &mut commands, &mut queue);
        admit_deferred(&mut state, &mut commands, &mut queue);
    }

    for scope in state.release_unneeded_scopes() {
        commands.push(Command::ReleaseScope { scope });
    }
    finish_if_quiescent(&mut state, &mut commands);
    (state, commands)
}

fn step(
    state: &mut EngineState,
    event: Event,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    if state.is_finished() {
        state.push_error(RunError::AlreadyFinished);
        return;
    }
    if !state.is_started() && !matches!(event, Event::RunStarted) {
        state.push_error(RunError::NotStarted);
        return;
    }

    match event {
        Event::RunStarted => on_run_started(state, queue),
        Event::TokenEmitted(token) => on_token(state, token, cmds, queue),
        Event::StepStarted { firing } => {
            if let Some(f) = state.firing_mut(firing) {
                f.started = true;
            } else {
                state.push_error(RunError::UnknownFiring(firing));
            }
        }
        // Progress is observation only: logs and artifacts carry no coordination
        // meaning, so the core records them and changes nothing.
        Event::StepProgress { .. } => {}
        Event::StepFinished { firing, outcome } => on_step_finished(state, firing, outcome, queue),
        Event::NodeExpanded { node, splice } => on_node_expanded(state, node, splice, queue),
        Event::CancelRequested { scope } => on_cancel(state, scope, cmds),
    }
}

// ── Run start ─────────────────────────────────────────────────────────────

fn on_run_started(state: &mut EngineState, queue: &mut VecDeque<Event>) {
    state.mark_started();
    // Entry nodes have no incoming edges, so each gets a synthetic one. Their join
    // then works like any other node's, with no special case in the firing rule.
    let entries = state.graph.entry.clone();
    for entry in entries {
        if state.graph.node(entry).is_none() {
            state.push_error(RunError::UnknownNode(entry));
            continue;
        }
        let edge = state.next_edge_id();
        state.register_seed_edge(edge, entry);
        queue.push_back(Event::TokenEmitted(Token::seeded(
            edge,
            Generation::ZERO,
            Value::Null,
        )));
    }
}

// ── Tokens and the firing rule ────────────────────────────────────────────

fn on_token(
    state: &mut EngineState,
    token: Token,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    let Some(target) = state.edge_target(token.edge) else {
        state.push_error(RunError::UnknownEdge(token.edge));
        return;
    };
    if state.graph.node(target).is_none() {
        state.push_error(RunError::UnknownNode(target));
        return;
    }
    // A cancelled scope swallows tokens aimed inside it.
    if state.is_node_cancelled(target) {
        return;
    }
    let key = (target, token.generation);
    // `Any` fires on the first token; later same-generation tokens are dropped.
    // The same rule stops a satisfied `All` from firing twice.
    if state.has_fired(key) {
        return;
    }
    state.store_token(target, token);
    try_fire(state, key, cmds, queue);
}

/// Check one `(node, generation)` and fire it if its join is satisfied.
fn try_fire(
    state: &mut EngineState,
    key: (NodeId, Generation),
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    let (node_id, generation) = key;
    if state.has_fired(key) || state.is_superseded(node_id) {
        return;
    }
    let Some(node) = state.graph.node(node_id).cloned() else {
        state.push_error(RunError::UnknownNode(node_id));
        return;
    };
    if !join_satisfied(state, &node, key) {
        return;
    }

    // Admission control: a splice with `max_parallel` holds surplus clones back
    // until a sibling finishes. The tokens stay pending, so nothing is lost.
    if let Some(splice) = state.splice_for_node(node_id)
        && let Some(max) = splice.max_parallel
        && state.live_in_splice(splice) >= max.max(1)
    {
        state.defer(key);
        return;
    }

    if state.firing_count(node_id) >= node.budget.max_firings {
        state.push_error(RunError::BudgetExceeded {
            node: node_id,
            max_firings: node.budget.max_firings,
        });
        state.take_tokens(key);
        state.mark_fired(key);
        return;
    }

    let inputs = state.take_tokens(key);
    state.mark_fired(key);
    state.bump_firing_count(node_id);

    if node.expand.is_some() {
        expand(state, &node, generation, &inputs, queue);
        return;
    }

    let ctx = match firing_context(state, node_id, &inputs, generation) {
        Ok(ctx) => ctx,
        Err(error) => {
            state.push_error(error);
            complete_without_running(
                state,
                &node,
                generation,
                &inputs,
                Outcome::failure("could not build the firing context"),
                queue,
            );
            return;
        }
    };

    // A precondition that is false skips the node, but routing still runs, so
    // `always()` and `failure()` guards downstream still see it.
    if let Some(precondition) = node.precondition {
        match eval_bool(&state.graph.exprs, precondition, &ctx) {
            Ok(true) => {}
            Ok(false) => {
                complete_without_running(
                    state,
                    &node,
                    generation,
                    &inputs,
                    synthetic(Status::Skipped),
                    queue,
                );
                return;
            }
            Err(error) => {
                state.push_error(RunError::Eval {
                    node: node_id,
                    site: SmolStr::new("precondition"),
                    error,
                });
                complete_without_running(
                    state,
                    &node,
                    generation,
                    &inputs,
                    Outcome::failure("precondition failed to evaluate"),
                    queue,
                );
                return;
            }
        }
    }

    let config = match resolve_config(&node.step.config, &state.graph.exprs, &ctx) {
        Ok(config) => config,
        Err(error) => {
            state.push_error(RunError::Eval {
                node: node_id,
                site: SmolStr::new("step config"),
                error,
            });
            complete_without_running(
                state,
                &node,
                generation,
                &inputs,
                Outcome::failure("step config failed to resolve"),
                queue,
            );
            return;
        }
    };

    let firing = Firing {
        id: state.next_firing_id(),
        node: node_id,
        generation,
        scope: node.scope,
        cancel_scope: state.cancel_scope_of(node_id),
        inputs: inputs.clone(),
        started: false,
        cancelling: false,
    };
    if state.acquire_scope(node.scope) {
        cmds.push(Command::AcquireScope { scope: node.scope });
    }
    cmds.push(Command::StartStep {
        firing: firing.id,
        node: node_id,
        generation,
        inputs,
        scope: node.scope,
        config,
    });
    state.insert_firing(firing);
}

fn join_satisfied(state: &EngineState, node: &Node, key: (NodeId, Generation)) -> bool {
    let Some(tokens) = state.tokens_for(key) else {
        return false;
    };
    if tokens.is_empty() {
        return false;
    }
    // Incoming edges are counted as of now, so edges a splice added are included.
    let incoming = state.incoming_edges(node.id);
    match node.join {
        JoinPolicy::All => incoming.iter().all(|edge| tokens.contains_key(edge)),
        JoinPolicy::Any => true,
        JoinPolicy::Quorum { n } => tokens.len() as u32 >= n.max(1),
    }
}

/// Record an outcome for a node that never executed, then route it.
fn complete_without_running(
    state: &mut EngineState,
    node: &Node,
    generation: Generation,
    inputs: &[Token],
    outcome: Outcome,
    queue: &mut VecDeque<Event>,
) {
    let firing = state.next_firing_id();
    state.record_outcome(FiringRecord {
        firing,
        node: node.id,
        name: node.name.clone(),
        generation,
        outcome: outcome.clone(),
    });
    route(state, node, firing, generation, inputs, &outcome, queue);
}

// ── Step completion ───────────────────────────────────────────────────────

fn on_step_finished(
    state: &mut EngineState,
    firing_id: FiringId,
    outcome: Outcome,
    queue: &mut VecDeque<Event>,
) {
    let Some(firing) = state.remove_firing(firing_id) else {
        state.push_error(RunError::UnknownFiring(firing_id));
        return;
    };
    let Some(node) = state.graph.node(firing.node).cloned() else {
        state.push_error(RunError::UnknownNode(firing.node));
        return;
    };
    state.record_outcome(FiringRecord {
        firing: firing_id,
        node: firing.node,
        name: node.name.clone(),
        generation: firing.generation,
        outcome: outcome.clone(),
    });

    // fail_fast: the first clone failure cancels its siblings through the splice's
    // own cancel scope.
    if outcome.status.is_failure()
        && let Some(splice) = state.splice_for_node(firing.node)
        && splice.fail_fast
        && !state.is_scope_cancelled(splice.cancel_scope)
    {
        let scope = splice.cancel_scope;
        queue.push_back(Event::CancelRequested { scope });
    }

    // A cancelled firing does not route: its tokens would restart work the cancel
    // was meant to stop.
    if firing.cancelling || state.is_node_cancelled(firing.node) {
        return;
    }
    route(
        state,
        &node,
        firing_id,
        firing.generation,
        &firing.inputs,
        &outcome,
        queue,
    );
}

// ── Routing ───────────────────────────────────────────────────────────────

/// Evaluate a node's routing: each group emits at most one token, and groups emit
/// concurrently.
fn route(
    state: &mut EngineState,
    node: &Node,
    firing: FiringId,
    generation: Generation,
    inputs: &[Token],
    outcome: &Outcome,
    queue: &mut VecDeque<Event>,
) {
    if node.routing.groups.is_empty() {
        return;
    }
    let base = match firing_context(state, node.id, inputs, generation) {
        Ok(ctx) => ctx,
        Err(error) => {
            state.push_error(error);
            return;
        }
    };
    let ctx = outcome_context(&base, outcome);

    for (group_index, group) in node.routing.groups.iter().enumerate() {
        let mut matched = false;
        for arm in &group.arms {
            let passes = match arm.guard {
                Guard::Always => true,
                Guard::Expr(id) => match eval_bool(&state.graph.exprs, id, &ctx) {
                    Ok(value) => value,
                    Err(error) => {
                        state.push_error(RunError::Eval {
                            node: node.id,
                            site: SmolStr::new(format!("guard on edge {}", arm.id)),
                            error,
                        });
                        false
                    }
                },
            };
            if !passes {
                continue;
            }
            let payload = match arm.map {
                None => outcome.output.clone(),
                Some(map) => match eval(&state.graph.exprs, map, &ctx) {
                    Ok(value) => value,
                    Err(error) => {
                        state.push_error(RunError::Eval {
                            node: node.id,
                            site: SmolStr::new(format!("map on edge {}", arm.id)),
                            error,
                        });
                        Value::Null
                    }
                },
            };
            // A back edge is what advances the loop counter. Nothing else does.
            let next_generation = if arm.back {
                generation.next()
            } else {
                generation
            };
            queue.push_back(Event::TokenEmitted(Token::new(
                arm.id,
                next_generation,
                payload,
                firing,
            )));
            matched = true;
            break;
        }
        if !matched && matches!(group.fallthrough, ir::Fallthrough::Error) {
            state.push_error(RunError::NoArmMatched {
                node: node.id,
                group: group_index,
            });
        }
    }
}

// ── Expansion ─────────────────────────────────────────────────────────────

/// Build the splice for a `for_each` node and hand it back as an event, so the
/// clones appear in the log exactly as they appear in the graph.
fn expand(
    state: &mut EngineState,
    node: &Node,
    generation: Generation,
    inputs: &[Token],
    queue: &mut VecDeque<Event>,
) {
    let Some(Expansion::ForEach {
        items,
        target,
        max_parallel,
        fail_fast,
    }) = node.expand.clone()
    else {
        return;
    };

    let ctx = match firing_context(state, node.id, inputs, generation) {
        Ok(ctx) => ctx,
        Err(error) => {
            state.push_error(error);
            return;
        }
    };
    let items = match eval(&state.graph.exprs, items, &ctx) {
        Ok(value) => value,
        Err(error) => {
            state.push_error(RunError::Eval {
                node: node.id,
                site: SmolStr::new("for_each items"),
                error,
            });
            return;
        }
    };
    let Value::Array(items) = items else {
        state.push_error(RunError::ItemsNotArray {
            node: node.id,
            got: SmolStr::new(type_name(&items)),
        });
        return;
    };

    // The region to clone. `ExpandTarget::Node` is the one-node case of the same
    // rule, so both go through one code path.
    let region: Vec<NodeId> = match target {
        ExpandTarget::Node => vec![node.id],
        ExpandTarget::Subgraph { entry, exit } => {
            if entry != node.id {
                state.push_error(RunError::ExpansionEntryMismatch { node: node.id });
                return;
            }
            region_nodes(state, entry, exit)
        }
    };
    let region_entry = node.id;

    let payload = inputs
        .first()
        .map(|t| t.payload.clone())
        .unwrap_or(Value::Null);

    let mut next_node = state.graph.nodes.len() as u32;
    let mut clones = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let index = index as u32;
        // Old id to new id, decided before any edge is rewritten so edges inside
        // the region point at clones and edges leaving it still point outward.
        let mut remap: BTreeMap<NodeId, NodeId> = BTreeMap::new();
        for old in &region {
            remap.insert(*old, NodeId::new(next_node));
            next_node += 1;
        }

        let mut nodes = Vec::with_capacity(region.len());
        for old in &region {
            let Some(source) = state.graph.node(*old) else {
                continue;
            };
            let mut clone = source.clone();
            clone.id = remap[old];
            clone.name = SmolStr::new(format!("{}#{index}", source.name));
            // Clones never expand again; that is what makes expansion terminate.
            clone.expand = None;
            for group in &mut clone.routing.groups {
                for arm in &mut group.arms {
                    arm.id = state.next_edge_id();
                    if let Some(new_target) = remap.get(&arm.to) {
                        arm.to = *new_target;
                    }
                }
            }
            nodes.push(clone);
        }

        let entry = remap[&region_entry];
        let seed_edge = state.next_edge_id();
        clones.push(SpliceClone {
            index,
            item: item.clone(),
            nodes,
            entry,
            seed_edge,
        });
    }

    let splice = SubgraphSplice {
        cancel_scope: state.next_cancel_scope_id(),
        source: node.id,
        region: region.clone(),
        generation,
        payload,
        clones,
        max_parallel,
        fail_fast,
    };
    queue.push_back(Event::NodeExpanded {
        node: node.id,
        splice,
    });
}

/// Nodes reachable from `entry` without passing through `exit`, plus `exit`.
/// Validation has already checked that the exit postdominates the entry.
fn region_nodes(state: &EngineState, entry: NodeId, exit: NodeId) -> Vec<NodeId> {
    let mut seen = BTreeSet::from([entry]);
    let mut order = vec![entry];
    let mut queue = VecDeque::from([entry]);
    while let Some(node) = queue.pop_front() {
        if node == exit {
            continue;
        }
        let Some(nd) = state.graph.node(node) else {
            continue;
        };
        for edge in nd.routing.edges() {
            if state.graph.node(edge.to).is_some() && seen.insert(edge.to) {
                order.push(edge.to);
                queue.push_back(edge.to);
            }
        }
    }
    order
}

fn on_node_expanded(
    state: &mut EngineState,
    source: NodeId,
    splice: SubgraphSplice,
    queue: &mut VecDeque<Event>,
) {
    let parent = state.cancel_scope_of(source);
    let mut scope_nodes = BTreeSet::new();

    for clone in &splice.clones {
        let bindings = clone_bindings(clone.index, &clone.item);
        for node in &clone.nodes {
            // Ids were allocated against this exact length, so a mismatch means the
            // splice was built against a different graph.
            debug_assert_eq!(node.id.index(), state.graph.nodes.len());
            state.graph.nodes.push(node.clone());
            state.set_clone_bindings(node.id, bindings.clone());
            state.set_node_cancel_scope(node.id, splice.cancel_scope);
            scope_nodes.insert(node.id);
        }
        state.register_seed_edge(clone.seed_edge, clone.entry);
    }

    state.add_cancel_scope(splice.cancel_scope, parent, scope_nodes.clone());
    for node in &splice.region {
        state.supersede(*node);
    }
    state.push_splice(Splice {
        cancel_scope: splice.cancel_scope,
        source,
        max_parallel: splice.max_parallel,
        fail_fast: splice.fail_fast,
        nodes: scope_nodes,
    });

    for clone in &splice.clones {
        queue.push_back(Event::TokenEmitted(Token::seeded(
            clone.seed_edge,
            splice.generation,
            splice.payload.clone(),
        )));
    }
}

// ── Cancellation ──────────────────────────────────────────────────────────

fn on_cancel(state: &mut EngineState, scope: CancelScopeId, cmds: &mut Vec<Command>) {
    let closure = state.cancel_scope_closure(scope);
    for id in &closure {
        state.mark_scope_cancelled(*id);
    }

    let root = scope == CancelScopeId::ROOT;
    if root {
        state.mark_cancelled();
        state.drop_all_tokens();
    } else {
        let nodes = state.nodes_in_scopes(&closure);
        state.drop_tokens_for_nodes(&nodes);
    }

    let doomed: Vec<FiringId> = state
        .live_firings()
        .filter(|f| root || closure.contains(&f.cancel_scope))
        .map(|f| f.id)
        .collect();
    for firing in doomed {
        if let Some(f) = state.firing_mut(firing) {
            if f.cancelling {
                continue;
            }
            f.cancelling = true;
        }
        cmds.push(Command::DeliverControl {
            firing,
            ctl: Control::Cancel,
        });
    }
}

// ── Admission control and completion ──────────────────────────────────────

fn admit_deferred(state: &mut EngineState, cmds: &mut Vec<Command>, queue: &mut VecDeque<Event>) {
    let held = state.take_deferred();
    for key in held {
        try_fire(state, key, cmds, queue);
    }
}

fn finish_if_quiescent(state: &mut EngineState, cmds: &mut Vec<Command>) {
    if !state.is_started() || state.is_finished() || !state.is_quiescent() {
        return;
    }
    state.mark_finished();
    cmds.push(Command::FinishRun {
        status: state.folded_status(),
    });
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
