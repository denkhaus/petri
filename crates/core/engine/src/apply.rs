//! The state machine itself.
//!
//! `apply` is deterministic: no IO, no clocks, no randomness. Timeouts and step
//! results arrive as events. Routing emits tokens by feeding events back to itself
//! through an internal queue, and every one of those is logged before it is applied,
//! so the log is a complete, replayable account of the run.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ir::{
    Attempt, CancelScopeId, Control, EvalEnv, Exhaustion, ExpandTarget, Expansion, FiringId,
    Generation, Guard, JoinPolicy, Node, NodeId, Outcome, Status, Token, Value, eval, eval_bool,
};
use smol_str::SmolStr;

use crate::context::{clone_bindings, firing_statics, primary_token, resolve_config, with_outcome};
use crate::event::{Command, Event, ResolvedFiring, SpliceClone, SubgraphSplice};
use crate::log::EventSource;
use crate::state::{EngineState, Firing, FiringRecord, RunError, Splice, synthetic};

/// Apply one event and return the commands it produced.
///
/// The event may cause the core to emit further events of its own (tokens, splices,
/// cascading cancellations). Those are drained inside this call, so the caller only
/// ever feeds in events that came from outside.
pub fn apply(mut state: EngineState, ev: Event) -> (EngineState, Vec<Command>) {
    let mut commands = Vec::new();
    let mut queue: VecDeque<Event> = VecDeque::from([ev]);
    // The first event came from outside; everything the drain adds is the core's own.
    let mut source = EventSource::External;

    loop {
        while let Some(event) = queue.pop_front() {
            state.log.append(source, event.clone());
            source = EventSource::Core;
            step(&mut state, event, &mut commands, &mut queue);
        }
        // Deferred joins are re-checked only once the queue is fully drained, so an
        // admission decision never runs ahead of an event already in flight — a
        // fail_fast cancel queued by a clone's failure must land before that
        // clone's deferred siblings are considered.
        admit_deferred(&mut state, &mut commands, &mut queue);
        if queue.is_empty() {
            break;
        }
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
        Event::StepStarted { firing, attempt } => match state.firing_mut(firing) {
            Some(f) if f.attempt == attempt => f.started = true,
            Some(f) => {
                let running = f.attempt;
                state.push_error(RunError::AttemptMismatch {
                    firing,
                    reported: attempt,
                    running,
                });
            }
            None => state.push_error(RunError::UnknownFiring(firing)),
        },
        // Progress is observation only: logs and artifacts carry no coordination
        // meaning, so the core records them and changes nothing.
        Event::StepProgress { .. } => {}
        Event::StepFinished {
            firing,
            attempt,
            outcome,
        } => on_step_finished(state, firing, attempt, outcome, cmds, queue),
        Event::RetryElapsed {
            firing,
            next_attempt,
        } => on_retry_elapsed(state, firing, next_attempt, cmds, queue),
        Event::NodeExpanded { node, splice } => on_node_expanded(state, node, splice, queue),
        Event::CancelRequested { scope } => on_cancel(state, scope, cmds, queue),
        Event::KillRequested { scope } => on_kill(state, scope, cmds, queue),
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
    // A killed scope swallows tokens aimed inside it. A merely cancelled scope does
    // not: its nodes receive their tokens and complete without executing — or fire,
    // when marked `run_on_cancel` — so `always()` and `cancelled()` cleanup can run.
    if state.is_node_killed(target) {
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

    // Nothing in a killed scope fires, ever. Its tokens were dropped and late ones
    // are swallowed before they get here; this covers whatever slipped in between.
    if state.is_node_killed(node_id) {
        return;
    }

    // §5: in a cancelled scope — or fed by cancelled work — only a node marked
    // `run_on_cancel` may actually run; everything else, expansions included,
    // completes `Cancelled` without evaluating anything, and routing still carries
    // the outcome onward. The input check is what keeps a `fail_fast` splice's
    // un-marked collector, which sits outside the cancelled scope, from starting.
    let cancelled = state.is_node_cancelled(node_id) || has_cancelled_input(state, key);
    let admitted = !cancelled || (node.run_on_cancel && node.expand.is_none());

    // Admission control: a splice with `max_parallel` holds surplus clones back
    // until a sibling finishes. The tokens stay pending, so nothing is lost. A
    // node completing without running takes no slot, so it is not held back.
    if admitted
        && let Some(splice) = state.splice_for_node(node_id)
        && let Some(max) = splice.max_parallel
        && state.live_in_splice(splice) >= max.max(1)
    {
        state.defer(key);
        return;
    }

    // The budget bounds synthesized outcomes too: an `Always`-guarded back edge
    // cycling through a cancelled region terminates here, exactly as a `Skipped`
    // cascade does.
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

    if !admitted {
        complete_without_running(
            state,
            &node,
            generation,
            &inputs,
            synthetic(Status::Cancelled),
            queue,
        );
        return;
    }

    if node.expand.is_some() {
        expand(state, &node, generation, &inputs, queue);
        return;
    }

    let statics = match firing_statics(state, node_id, &inputs, generation, Attempt::FIRST) {
        Ok(statics) => statics,
        Err(error) => {
            state.push_error(error);
            complete_without_running(
                state,
                &node,
                generation,
                &inputs,
                Outcome::failure("could not build the firing environment"),
                queue,
            );
            return;
        }
    };
    let token = primary_token(&inputs);

    // A precondition that is false skips the node, but routing still runs, so
    // `always()` and `failure()` guards downstream still see it. In a cancelled
    // scope the record says `Cancelled`, not `Skipped`: why the node did not run.
    // An evaluation error keeps its ordinary behavior either way — cancellation
    // must not convert a broken expression into a clean cancellation.
    if let Some(precondition) = node.precondition {
        let env = EvalEnv::new(&token, state.run_context(), &statics);
        match eval_bool(&state.graph.exprs, precondition, &env) {
            Ok(true) => {}
            Ok(false) => {
                let status = if cancelled {
                    Status::Cancelled
                } else {
                    Status::Skipped
                };
                complete_without_running(
                    state,
                    &node,
                    generation,
                    &inputs,
                    synthetic(status),
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

    let config = match resolve_config(
        &node.step.config,
        &state.graph.exprs,
        &EvalEnv::new(&token, state.run_context(), &statics),
    ) {
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

    let firing_id = state.next_firing_id();
    // The constructor is where "no unresolved ExprId crosses the executor boundary"
    // is enforced. A malformed placeholder that `resolve_config` could not read is
    // caught here rather than reaching a step.
    let resolved = match ResolvedFiring::new(
        firing_id,
        node_id,
        generation,
        Attempt::FIRST,
        node.scope,
        inputs.clone(),
        config,
    ) {
        Ok(resolved) => resolved,
        Err(unresolved) => {
            state.push_error(RunError::UnresolvedConfig {
                node: node_id,
                path: unresolved.path,
            });
            complete_without_running(
                state,
                &node,
                generation,
                &inputs,
                Outcome::failure("step config still holds an unresolved expression"),
                queue,
            );
            return;
        }
    };

    let firing = Firing {
        id: firing_id,
        node: node_id,
        generation,
        attempt: Attempt::FIRST,
        scope: node.scope,
        cancel_scope: state.cancel_scope_of(node_id),
        inputs,
        started: false,
        cancelling: false,
        awaiting_retry: false,
    };
    if state.acquire_scope(node.scope) {
        cmds.push(Command::AcquireScope { scope: node.scope });
    }
    cmds.push(Command::StartStep(resolved));
    state.insert_firing(firing);
}

/// Whether any token waiting at this join was emitted by a firing whose recorded
/// outcome is `Cancelled`. Work fed by cancelled work is admitted the same way as
/// work inside a cancelled scope: only via `run_on_cancel`.
fn has_cancelled_input(state: &EngineState, key: (NodeId, Generation)) -> bool {
    let Some(tokens) = state.tokens_for(key) else {
        return false;
    };
    tokens.values().any(|t| state.outcome_was_cancelled(t.from))
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
        attempt: Attempt::FIRST,
        outcome: outcome.clone(),
    });
    route(
        state,
        node,
        firing,
        generation,
        Attempt::FIRST,
        inputs,
        &outcome,
        queue,
    );
}

// ── Step completion ───────────────────────────────────────────────────────

fn on_step_finished(
    state: &mut EngineState,
    firing_id: FiringId,
    attempt: Attempt,
    outcome: Outcome,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    let Some(firing) = state.firing(firing_id).cloned() else {
        state.push_error(RunError::UnknownFiring(firing_id));
        return;
    };
    if firing.attempt != attempt {
        state.push_error(RunError::AttemptMismatch {
            firing: firing_id,
            reported: attempt,
            running: firing.attempt,
        });
        return;
    }
    let Some(node) = state.graph.node(firing.node).cloned() else {
        state.push_error(RunError::UnknownNode(firing.node));
        return;
    };

    let cancelled = firing.cancelling || state.is_node_cancelled(firing.node);

    // Retry decision. A cancelled firing is never retried: the point of cancelling
    // is to stop the work, not to start it again.
    if !cancelled
        && node.retry.should_retry(&outcome.status)
        && node.retry.has_attempt_after(attempt)
    {
        // The firing stays live, so its scope stays held and the run stays
        // non-quiescent while the driver waits out the backoff. Nothing is recorded
        // and nothing is routed: only the final attempt is visible downstream.
        if let Some(f) = state.firing_mut(firing_id) {
            f.awaiting_retry = true;
            f.started = false;
        }
        cmds.push(Command::ScheduleRetry {
            firing: firing_id,
            next_attempt: attempt.next(),
            base_delay: node.retry.base_delay(attempt),
        });
        return;
    }

    // This attempt is final.
    state.remove_firing(firing_id);
    let outcome = accept_partial_on_exhaustion(&node, attempt, outcome);

    state.record_outcome(FiringRecord {
        firing: firing_id,
        node: firing.node,
        name: node.name.clone(),
        generation: firing.generation,
        attempt,
        outcome: outcome.clone(),
    });

    // fail_fast: the first clone failure cancels its siblings through the splice's
    // own cancel scope. Keyed off the final attempt, like everything else.
    if outcome.status.is_failure()
        && let Some(splice) = state.splice_for_node(firing.node)
        && splice.fail_fast
        && !state.is_scope_cancelled(splice.cancel_scope)
    {
        let scope = splice.cancel_scope;
        queue.push_back(Event::CancelRequested { scope });
    }

    // A killed firing's outcome is recorded but never routed. A merely cancelled
    // one routes like any other outcome (§5): what stops work from restarting is
    // the structural `run_on_cancel` admission in `try_fire`, not a routing hole.
    if state.is_node_killed(firing.node) {
        return;
    }
    route(
        state,
        &node,
        firing_id,
        firing.generation,
        attempt,
        &firing.inputs,
        &outcome,
        queue,
    );
}

/// Turn an exhausted retry's failure into a `PartialSuccess`, keeping the real
/// failure in `underlying` so the log never records a clean success for something
/// that failed.
fn accept_partial_on_exhaustion(node: &Node, attempt: Attempt, outcome: Outcome) -> Outcome {
    let exhausted =
        node.retry.should_retry(&outcome.status) && !node.retry.has_attempt_after(attempt);
    if node.retry.on_exhaustion != Exhaustion::AcceptPartial || !exhausted {
        return outcome;
    }
    let underlying = outcome.status.failure_info().cloned();
    Outcome {
        status: Status::PartialSuccess { underlying },
        ..outcome
    }
}

/// The backoff elapsed: start the next attempt.
///
/// The config is resolved again, so a retry sees the run context as it stands now
/// rather than as it stood before the first attempt.
fn on_retry_elapsed(
    state: &mut EngineState,
    firing_id: FiringId,
    next_attempt: Attempt,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    // A cancel or kill settled this firing while it was waiting out its backoff:
    // the driver's sleeper could not be recalled, so this arrival was expected.
    // The no-op is tombstone-precise; any other invalid `RetryElapsed` — unknown
    // firing, not awaiting, duplicate after the tombstone is consumed — still
    // errors below, so malformed external input stays loud.
    if state.take_retry_tombstone(firing_id) {
        return;
    }
    let Some(firing) = state.firing(firing_id).cloned() else {
        state.push_error(RunError::UnknownFiring(firing_id));
        return;
    };
    if !firing.awaiting_retry {
        state.push_error(RunError::UnexpectedRetry { firing: firing_id });
        return;
    }
    let Some(node) = state.graph.node(firing.node).cloned() else {
        state.push_error(RunError::UnknownNode(firing.node));
        return;
    };

    let statics = match firing_statics(
        state,
        firing.node,
        &firing.inputs,
        firing.generation,
        next_attempt,
    ) {
        Ok(statics) => statics,
        Err(error) => {
            state.push_error(error);
            fail_live_firing(state, firing_id, &node, &firing, next_attempt, queue);
            return;
        }
    };
    let token = primary_token(&firing.inputs);
    let config = match resolve_config(
        &node.step.config,
        &state.graph.exprs,
        &EvalEnv::new(&token, state.run_context(), &statics),
    ) {
        Ok(config) => config,
        Err(error) => {
            state.push_error(RunError::Eval {
                node: firing.node,
                site: SmolStr::new("step config"),
                error,
            });
            fail_live_firing(state, firing_id, &node, &firing, next_attempt, queue);
            return;
        }
    };
    let resolved = match ResolvedFiring::new(
        firing_id,
        firing.node,
        firing.generation,
        next_attempt,
        firing.scope,
        firing.inputs.clone(),
        config,
    ) {
        Ok(resolved) => resolved,
        Err(unresolved) => {
            state.push_error(RunError::UnresolvedConfig {
                node: firing.node,
                path: unresolved.path,
            });
            fail_live_firing(state, firing_id, &node, &firing, next_attempt, queue);
            return;
        }
    };

    if let Some(f) = state.firing_mut(firing_id) {
        f.attempt = next_attempt;
        f.awaiting_retry = false;
    }
    cmds.push(Command::StartStep(resolved));
}

/// End a live firing that could not be restarted, recording the failure and routing
/// it like any other final outcome.
fn fail_live_firing(
    state: &mut EngineState,
    firing_id: FiringId,
    node: &Node,
    firing: &Firing,
    attempt: Attempt,
    queue: &mut VecDeque<Event>,
) {
    state.remove_firing(firing_id);
    let outcome = Outcome::failure("the retry could not be prepared");
    state.record_outcome(FiringRecord {
        firing: firing_id,
        node: firing.node,
        name: node.name.clone(),
        generation: firing.generation,
        attempt,
        outcome: outcome.clone(),
    });
    route(
        state,
        node,
        firing_id,
        firing.generation,
        attempt,
        &firing.inputs,
        &outcome,
        queue,
    );
}

// ── Routing ───────────────────────────────────────────────────────────────

/// Evaluate a node's routing: each group emits at most one token, and groups emit
/// concurrently.
#[allow(clippy::too_many_arguments)]
fn route(
    state: &mut EngineState,
    node: &Node,
    firing: FiringId,
    generation: Generation,
    attempt: Attempt,
    inputs: &[Token],
    outcome: &Outcome,
    queue: &mut VecDeque<Event>,
) {
    if node.routing.groups.is_empty() {
        return;
    }
    let base = match firing_statics(state, node.id, inputs, generation, attempt) {
        Ok(statics) => statics,
        Err(error) => {
            state.push_error(error);
            return;
        }
    };
    let statics = with_outcome(&base, outcome);
    let token = primary_token(inputs);

    for (group_index, group) in node.routing.groups.iter().enumerate() {
        let mut matched = false;
        for arm in &group.arms {
            let passes = match arm.guard {
                Guard::Always => true,
                Guard::Expr(id) => match eval_bool(
                    &state.graph.exprs,
                    id,
                    &EvalEnv::new(&token, state.run_context(), &statics),
                ) {
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
                Some(map) => match eval(
                    &state.graph.exprs,
                    map,
                    &EvalEnv::new(&token, state.run_context(), &statics),
                ) {
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

    let statics = match firing_statics(state, node.id, inputs, generation, Attempt::FIRST) {
        Ok(statics) => statics,
        Err(error) => {
            state.push_error(error);
            return;
        }
    };
    let token = primary_token(inputs);
    let items = match eval(
        &state.graph.exprs,
        items,
        &EvalEnv::new(&token, state.run_context(), &statics),
    ) {
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

/// The polite tier. Live firings get `Control::Cancel`; pending tokens survive, so
/// nodes in the scope complete `Cancelled` — or fire, when marked `run_on_cancel` —
/// as their joins satisfy (§5).
fn on_cancel(
    state: &mut EngineState,
    scope: CancelScopeId,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    stop_scope(state, scope, false, cmds, queue);
}

/// The forced tier: the pre-v3 cancel behavior, kept under its own event. Tokens
/// drop, nothing routes, nothing is admitted — `run_on_cancel` included — and
/// `Control::Kill` reaches every live firing, already-cancelling ones too.
fn on_kill(
    state: &mut EngineState,
    scope: CancelScopeId,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    stop_scope(state, scope, true, cmds, queue);
}

/// Both tiers share one shape — mark the closure, doom its live firings — and
/// differ only where the spec says they do: a kill drops tokens, routes nothing,
/// and reaches firings the polite tier already signalled.
fn stop_scope(
    state: &mut EngineState,
    scope: CancelScopeId,
    kill: bool,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    let closure = state.cancel_scope_closure(scope);
    for id in &closure {
        if kill {
            state.mark_scope_killed(*id);
        } else {
            state.mark_scope_cancelled(*id);
        }
    }
    let root = scope == CancelScopeId::ROOT;
    if root {
        state.mark_cancelled();
    }
    if kill {
        if root {
            state.drop_all_tokens();
        } else {
            let nodes = state.nodes_in_scopes(&closure);
            state.drop_tokens_for_nodes(&nodes);
        }
    }

    let doomed: Vec<Firing> = state
        .live_firings()
        .filter(|f| root || closure.contains(&f.cancel_scope))
        .cloned()
        .collect();
    for firing in doomed {
        // A firing waiting out a retry backoff has no work in flight and no driver
        // task to deliver to, so the core settles it at once: recorded — and, under
        // a cancel only, routed.
        if firing.awaiting_retry {
            settle_awaiting_retry(state, &firing, !kill, queue);
            continue;
        }
        if let Some(f) = state.firing_mut(firing.id) {
            // Only the polite tier skips already-cancelling firings: a plain Cancel
            // cannot say "skip the ladder", so a kill reaches them too.
            if !kill && f.cancelling {
                continue;
            }
            f.cancelling = true;
        }
        cmds.push(Command::DeliverControl {
            firing: firing.id,
            ctl: if kill { Control::Kill } else { Control::Cancel },
        });
    }
}

/// Settle a firing that a cancel or kill caught mid-backoff: record `Cancelled`
/// now — there is no task to deliver a control to — and leave a tombstone for the
/// driver's unrecallable `RetryElapsed`. Under a cancel the outcome routes; under a
/// kill it does not.
fn settle_awaiting_retry(
    state: &mut EngineState,
    firing: &Firing,
    routes: bool,
    queue: &mut VecDeque<Event>,
) {
    state.remove_firing(firing.id);
    state.add_retry_tombstone(firing.id);
    let Some(node) = state.graph.node(firing.node).cloned() else {
        state.push_error(RunError::UnknownNode(firing.node));
        return;
    };
    let outcome = synthetic(Status::Cancelled);
    state.record_outcome(FiringRecord {
        firing: firing.id,
        node: firing.node,
        name: node.name.clone(),
        generation: firing.generation,
        attempt: firing.attempt,
        outcome: outcome.clone(),
    });
    if routes {
        route(
            state,
            &node,
            firing.id,
            firing.generation,
            firing.attempt,
            &firing.inputs,
            &outcome,
            queue,
        );
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
    // Terminal release, in the same transition as the finish: a token parked at an
    // unsatisfiable join would otherwise hold its environment forever, and a
    // finished serialized state must claim no resources.
    for scope in state.release_all_scopes() {
        cmds.push(Command::ReleaseScope { scope });
    }
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
