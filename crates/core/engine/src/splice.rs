//! The splice boundary: preparation and the one applicator.
//!
//! Three representations, one direction of travel. A `SpliceRequest` is
//! fragment-local, serialized in `StepFinished`, and untrusted. Preparation
//! ([`prepare_outcome_splices`]) turns an ordered list of them into a
//! [`SplicePlan`] against a scratch view of `EngineState` — remapping every
//! identifier from live-graph high-water marks, resolving attachments, and
//! computing retraction — without touching canonical state, so a rejected
//! transaction leaks nothing, not even allocator movement. [`PreparedSplice`] is
//! engine-private, non-serializable, and constructible only here (or by the
//! `ForEach` producer); `apply_prepared_splice` is infallible for domain errors,
//! because validation is a type boundary, not a convention.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ir::{
    BinOp, CancelScopeId, Edge, EdgeId, Expr, ExprId, ExprOrValue, FailureInfo, Generation, Guard,
    JoinPolicy, Local, Node, NodeId, Outcome, Scope, ScopeId, SelectGroup, SpliceMode,
    SplicePolicy, SpliceRequest, Status, StepRef, Token, Value, validate_request,
};
use smol_str::SmolStr;

use crate::event::Event;
use crate::state::{
    AdmissionKey, Allocators, AppliedSplice, EngineState, SpliceBatchId, SpliceProducer,
};

/// The failure class every rejected splice transaction converts to. Registered
/// in the §13 table; an ordinary retry class — a matching `retry_on` re-runs
/// the step, and a later attempt can succeed.
pub const INVALID_SPLICE_CLASS: &str = "invalid_splice";

/// The canonical invalid-splice conversion: the outcome becomes a
/// `Failure{class: invalid_splice}` with `message`, output and metrics are kept,
/// and `context_updates` drop with the requests. Both converters — the engine's
/// preparation rejection and the driver's secret backstop — go through here, so
/// the rule cannot drift.
pub fn reject_splices(outcome: Outcome, message: impl Into<String>) -> Outcome {
    Outcome {
        status: Status::Failure(FailureInfo::new(message).with_class(INVALID_SPLICE_CLASS)),
        context_updates: BTreeMap::new(),
        splices: Vec::new(),
        ..outcome
    }
}

// ── Preparation errors ────────────────────────────────────────────────────

/// Why a splice transaction was rejected. Preparation wraps the fragment-local
/// [`ir::FragmentValidationError`] — which cannot know a request index — with
/// the index and phase; only the engine boundary (`on_step_finished`) maps this
/// to the canonical `Failure{class: invalid_splice}`.
#[derive(Clone, Debug, thiserror::Error)]
#[error("splice request {request_index} rejected during {}: {source}", .source.phase())]
pub(crate) struct SplicePreparationError {
    pub request_index: usize,
    pub source: PreparationRejection,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum PreparationRejection {
    #[error("{}", format_fragment_errors(.0))]
    Fragment(Vec<ir::FragmentValidationError>),
    #[error("the uploader's policy {policy:?} does not authorize {mode:?}")]
    Policy {
        policy: SplicePolicy,
        mode: SpliceMode,
    },
    #[error(
        "fragment node `{node}` declares policy {declared:?}, above its uploader's {cap:?}; \
         a fragment can never mint more authority than its uploader"
    )]
    Delegation {
        node: SmolStr,
        declared: SplicePolicy,
        cap: SplicePolicy,
    },
    #[error("instance name `{name}` collides with a node already in the graph")]
    NameCollision { name: SmolStr },
    #[error("`depends_on` names `{name}`, which no live or planned node carries")]
    UnknownReference { name: SmolStr },
    #[error(
        "`depends_on` target `{name}` has admissions in more than one generation; \
         a reference must resolve to exactly one admission, so loop nodes reject in v1"
    )]
    AmbiguousReference { name: SmolStr },
    #[error("`depends_on` target `{name}` was retracted by an earlier request in this outcome")]
    RetractedReference { name: SmolStr },
    #[error(
        "dependent `{name}` joins with {join:?}; only `All` joins can be extended to \
         wait for the batch in v1"
    )]
    DependentJoinNotAll { name: SmolStr, join: JoinPolicy },
}

impl PreparationRejection {
    /// Which preparation phase this rejection comes from — policy authorization,
    /// fragment-local validation, or composition with the live graph.
    fn phase(&self) -> &'static str {
        match self {
            Self::Policy { .. } | Self::Delegation { .. } => "policy",
            Self::Fragment(_) => "validation",
            _ => "composition",
        }
    }
}

fn format_fragment_errors(errors: &[ir::FragmentValidationError]) -> String {
    match errors.first() {
        Some(first) if errors.len() == 1 => first.to_string(),
        Some(first) => format!("{first} (and {} more)", errors.len() - 1),
        None => "invalid fragment".to_string(),
    }
}

// ── The prepared shape ────────────────────────────────────────────────────

/// One seed for a spliced entry: the synthetic incoming edge, the node it feeds,
/// and the token placed on it.
pub(crate) struct PreparedSeed {
    pub edge: EdgeId,
    pub entry: NodeId,
    pub generation: Generation,
    pub payload: Value,
}

/// A splice with every identifier remapped and every check complete: the only
/// input the applicator accepts.
pub(crate) struct PreparedSplice {
    /// The node whose firing produced this batch: the expansion source, or the
    /// uploader. Stamped on the record — ownership is core-derived.
    pub owner: NodeId,
    pub cancel_scope: CancelScopeId,
    /// The scope the fresh batch scope nests under: the producing firing's
    /// current cancel scope.
    pub parent_scope: CancelScopeId,
    /// Fully formed live-space nodes, ids contiguous with the live graph.
    pub nodes: Vec<Node>,
    /// Expressions appended to the live table, ids contiguous with it: the
    /// fragment's own, then the synthesized attachment guards.
    pub exprs: Vec<Expr>,
    /// Resource scopes appended to the live list, ids contiguous with it.
    pub scopes: Vec<Scope>,
    /// `item` / `index` bindings for expansion clones; empty for uploads.
    pub bindings: BTreeMap<NodeId, BTreeMap<SmolStr, Value>>,
    /// Seed tokens for entries fed by no real edge (`ForEach` clones). Uploaded
    /// fragments attach through real select groups instead and seed nothing.
    pub seeds: Vec<PreparedSeed>,
    /// Select groups appended to nodes once they are live: the uploader's entry
    /// groups, and `depends_on` edges on not-final or earlier-planned references.
    pub routing_extensions: Vec<(NodeId, SelectGroup)>,
    /// Producer-only behavior, applied here and recorded verbatim on the
    /// [`AppliedSplice`].
    pub producer: SpliceProducer,
}

/// Every request in one final outcome, prepared: the transaction plan. Commit
/// consumes it in order; nothing partial ever commits.
pub(crate) struct SplicePlan {
    pub batches: Vec<PreparedSplice>,
    pub allocators: Allocators,
}

// ── Preparation ───────────────────────────────────────────────────────────

/// Prepare every request in order against an evolving scratch view. Canonical
/// state is read, never written.
pub(crate) fn prepare_outcome_splices(
    state: &EngineState,
    uploader: &Node,
    generation: Generation,
    parent_scope: CancelScopeId,
    requests: &[SpliceRequest],
) -> Result<SplicePlan, SplicePreparationError> {
    let mut alloc = state.allocator_snapshot();
    let mut expr_cursor = state.graph.exprs.len() as u32;
    let mut scope_cursor = state.graph.scopes.len() as u32;
    let mut batches: Vec<PreparedSplice> = Vec::new();
    let mut retracted: BTreeSet<AdmissionKey> = BTreeSet::new();

    let mut names: BTreeMap<SmolStr, NodeId> = state
        .graph
        .nodes
        .iter()
        .map(|n| (n.name.clone(), n.id))
        .collect();
    // Canonical state never changes during preparation, so these are computed at
    // most once across the whole transaction — and not at all when no request
    // needs them.
    let loop_nodes: OnceCell<BTreeSet<NodeId>> = OnceCell::new();
    let pending_keys: OnceCell<Vec<AdmissionKey>> = OnceCell::new();
    let owned_nodes: OnceCell<BTreeSet<NodeId>> = OnceCell::new();
    let mut ambiguous_references: BTreeMap<NodeId, bool> = BTreeMap::new();

    for (request_index, request) in requests.iter().enumerate() {
        let reject = |source| SplicePreparationError {
            request_index,
            source,
        };

        // Policy authorization before anything else: `authorize` per request,
        // the delegation check per fragment node. Excess authority in either
        // direction rejects; nothing is clamped.
        if !uploader.splice_policy.authorizes(&request.mode) {
            return Err(reject(PreparationRejection::Policy {
                policy: uploader.splice_policy,
                mode: request.mode,
            }));
        }
        for fragment_node in &request.fragment.nodes {
            if !uploader
                .splice_policy
                .may_delegate(fragment_node.splice_policy)
            {
                return Err(reject(PreparationRejection::Delegation {
                    node: fragment_node.name.clone(),
                    declared: fragment_node.splice_policy,
                    cap: uploader.splice_policy,
                }));
            }
        }

        // Fragment-local validation: the shared invariant engine over the
        // fragment view, plus the mode/emptiness rule.
        if let Err(errors) = validate_request(request) {
            return Err(reject(PreparationRejection::Fragment(errors)));
        }

        // Retraction, before attachment: computed from state at finalization
        // time, filtered by the request's scope against recorded batch owners.
        // Keys another request already took are skipped, so the plan stays a
        // set.
        let mut batch_retracted: Vec<AdmissionKey> = Vec::new();
        if let SpliceMode::Replace { scope } = request.mode {
            for key in pending_keys.get_or_init(|| state.pending_admission_keys()) {
                if retracted.contains(key) {
                    continue;
                }
                let qualifies = match scope {
                    ir::ReplaceScope::AllPending => true,
                    ir::ReplaceScope::OwnBatches => owned_nodes
                        .get_or_init(|| {
                            state
                                .splices()
                                .iter()
                                .filter(|b| b.owner == uploader.id)
                                .flat_map(|b| b.nodes.iter().copied())
                                .collect()
                        })
                        .contains(&key.node),
                };
                if qualifies {
                    retracted.insert(*key);
                    batch_retracted.push(*key);
                }
            }
        }

        // Identifier remapping from live-graph high-water marks: nodes, edges,
        // scopes and expressions all shift into freshly allocated live ids.
        let fragment = &request.fragment;
        let node_base = alloc.reserve_nodes(fragment.nodes.len());
        let expr_base = expr_cursor;
        let scope_base = scope_cursor;

        let mut exprs: Vec<Expr> = fragment
            .exprs
            .iter()
            .map(|(_, expr)| shift_expr(expr, expr_base))
            .collect();
        let scopes: Vec<Scope> = fragment
            .scopes
            .iter()
            .map(|scope| remap_scope(scope, scope_base, expr_base))
            .collect();
        let mut nodes: Vec<Node> = fragment
            .nodes
            .iter()
            .map(|node| remap_node(node, node_base, scope_base, expr_base, &mut alloc))
            .collect();

        // Instance names are used verbatim; a collision with any live name, or
        // with a name planned earlier in this transaction, rejects.
        for node in &nodes {
            if names.contains_key(&node.name) {
                return Err(reject(PreparationRejection::NameCollision {
                    name: node.name.clone(),
                }));
            }
            names.insert(node.name.clone(), node.id);
        }

        // Entry attachment: one new select group on the uploader per entry,
        // guarded success-like, so uploaded work starts only from a
        // success-like uploader — and pinned to the uploading firing's
        // generation, because the groups persist on the node: a loop-head
        // uploader's next generation must not re-seed an earlier batch.
        // Added pre-routing — the uploader's own routing pass runs after
        // commit and evaluates these.
        let mut extensions: Vec<(NodeId, SelectGroup)> = Vec::new();
        if !fragment.entries.is_empty() {
            let outcome_var = push_expr(&mut exprs, expr_base, Expr::Var(SmolStr::new("outcome")));
            let success_like = push_expr(
                &mut exprs,
                expr_base,
                Expr::Field(outcome_var, SmolStr::new("success_like")),
            );
            let generation_var =
                push_expr(&mut exprs, expr_base, Expr::Var(SmolStr::new("generation")));
            let this_generation = push_expr(
                &mut exprs,
                expr_base,
                Expr::Lit(Value::from(generation.raw())),
            );
            let same_generation = push_expr(
                &mut exprs,
                expr_base,
                Expr::Binary(BinOp::Eq, generation_var, this_generation),
            );
            let guard = push_expr(
                &mut exprs,
                expr_base,
                Expr::Binary(BinOp::And, success_like, same_generation),
            );
            for entry in &fragment.entries {
                let to = NodeId::new(node_base + entry.raw());
                extensions.push((
                    uploader.id,
                    SelectGroup::new(vec![Edge::when(alloc.take_edge(), to, guard)]),
                ));
            }
        }

        // Dependent auto-extension: each existing forward dependent of the
        // uploader also waits for the batch, via edges from every exit. Such a
        // dependent cannot have fired — its `All` join still awaits the
        // uploader's token — and anything but `All` rejects (a generated batch
        // barrier for `Any`/`Quorum` is a v2 seam). A dependent this
        // transaction already retracted is skipped: it can never fire.
        if !fragment.exits.is_empty() {
            let mut dependents: Vec<NodeId> = Vec::new();
            let mut seen_dependents = BTreeSet::new();
            for edge in uploader.routing.edges().filter(|e| !e.back) {
                if seen_dependents.insert(edge.to)
                    && state.graph.node(edge.to).is_some()
                    && !state.is_superseded(edge.to)
                {
                    dependents.push(edge.to);
                }
            }
            for dependent in dependents {
                if retracted.contains(&AdmissionKey {
                    node: dependent,
                    generation,
                }) {
                    continue;
                }
                let dependent_node = state.graph.node(dependent).expect("checked above");
                if dependent_node.join != JoinPolicy::All {
                    return Err(reject(PreparationRejection::DependentJoinNotAll {
                        name: dependent_node.name.clone(),
                        join: dependent_node.join,
                    }));
                }
                for exit in &fragment.exits {
                    nodes[exit.index()]
                        .routing
                        .groups
                        .push(SelectGroup::new(vec![Edge::always(
                            alloc.take_edge(),
                            dependent,
                        )]));
                }
            }
        }

        // Cross-batch `depends_on`, by state at splice time. A not-final
        // reference extends the referenced node's routing with a real edge —
        // it has not routed its final outcome yet, so the edge can still emit.
        // A final reference becomes a guard over `nodes.<name>.status`: the
        // record already exists, so the guard does not need to wait. Both are
        // completion ordering; status gating stays the frontend's business.
        for attachment in &request.attachments {
            let ir::Attachment::DependsOn { node, on } = attachment;
            let dependent_index = node.index();
            let dependent_live = NodeId::new(node_base + node.raw());
            let name = SmolStr::new(on.as_str());
            let Some(&target) = names.get(name.as_str()) else {
                return Err(reject(PreparationRejection::UnknownReference { name }));
            };
            if state.graph.node(target).is_some() {
                if retracted.iter().any(|key| key.node == target) {
                    return Err(reject(PreparationRejection::RetractedReference { name }));
                }
                // Exactly one admission: a loop node — or anything already
                // admitted in more than one generation — is ambiguous, so
                // the final/not-final rule below never is.
                let ambiguous = *ambiguous_references.entry(target).or_insert_with(|| {
                    loop_nodes
                        .get_or_init(|| ir::validate::loop_reachable(&state.graph))
                        .contains(&target)
                        || state.has_multiple_admission_generations(target)
                });
                if ambiguous {
                    return Err(reject(PreparationRejection::AmbiguousReference { name }));
                }
                if state.run_context().node(name.as_str()).is_some() {
                    let guard = record_exists_guard(&mut exprs, expr_base, &name);
                    conjoin_precondition(&mut nodes[dependent_index], &mut exprs, expr_base, guard);
                    continue;
                }
            }
            // A non-final live node or any planned node gets one routing
            // extension. Batches apply in order, after adding their nodes, so the
            // target is live before this extension is committed.
            extensions.push((
                target,
                SelectGroup::new(vec![Edge::always(alloc.take_edge(), dependent_live)]),
            ));
        }

        expr_cursor = expr_base + exprs.len() as u32;
        scope_cursor = scope_base + scopes.len() as u32;

        batches.push(PreparedSplice {
            owner: uploader.id,
            cancel_scope: alloc.take_cancel_scope(),
            parent_scope,
            nodes,
            exprs,
            scopes,
            bindings: BTreeMap::new(),
            seeds: Vec::new(),
            routing_extensions: extensions,
            producer: SpliceProducer::Outcome {
                retracted: batch_retracted,
            },
        });
    }

    Ok(SplicePlan {
        batches,
        allocators: alloc,
    })
}

/// Commit a prepared transaction: adopt the allocator movement, then apply the
/// batches in order. Nothing here can fail; nothing partial ever commits.
pub(crate) fn commit_splice_plan(
    state: &mut EngineState,
    plan: SplicePlan,
    queue: &mut VecDeque<Event>,
) {
    state.adopt_allocators(plan.allocators);
    for prepared in plan.batches {
        apply_prepared_splice(state, prepared, queue);
    }
}

// ── The applicator ────────────────────────────────────────────────────────

/// Apply one prepared splice: mutate the live graph, record the batch, seed the
/// entries. Infallible — everything that can be refused was refused during
/// preparation, before this type could exist.
pub(crate) fn apply_prepared_splice(
    state: &mut EngineState,
    prepared: PreparedSplice,
    queue: &mut VecDeque<Event>,
) {
    for expr in prepared.exprs {
        state.graph.exprs.push(expr);
    }
    for scope in prepared.scopes {
        debug_assert_eq!(scope.id.index(), state.graph.scopes.len());
        state.graph.scopes.push(scope);
    }

    let mut batch_nodes = BTreeSet::new();
    let mut bindings = prepared.bindings;
    for node in prepared.nodes {
        // Ids were allocated against this exact length, so a mismatch means the
        // splice was built against a different graph.
        debug_assert_eq!(node.id.index(), state.graph.nodes.len());
        let id = node.id;
        state.graph.nodes.push(node);
        if let Some(node_bindings) = bindings.remove(&id) {
            state.set_clone_bindings(id, node_bindings);
        }
        state.set_node_cancel_scope(id, prepared.cancel_scope);
        batch_nodes.insert(id);
    }
    for seed in &prepared.seeds {
        state.register_seed_edge(seed.edge, seed.entry);
    }
    for (target, group) in prepared.routing_extensions {
        if let Some(node) = state.graph.node_mut(target) {
            node.routing.groups.push(group);
        }
    }

    state.add_cancel_scope(
        prepared.cancel_scope,
        prepared.parent_scope,
        batch_nodes.clone(),
    );

    match &prepared.producer {
        SpliceProducer::ForEach { superseded, .. } => {
            for node in superseded {
                state.supersede(*node);
            }
        }
        SpliceProducer::Outcome { retracted } => {
            state.retract_admissions(retracted);
        }
    }

    // Batch identity is apply-time bookkeeping, like ownership: the id is the
    // record's position in the applied list, stamped here for both producers.
    state.push_splice(AppliedSplice {
        batch: SpliceBatchId(state.splices().len() as u32),
        owner: prepared.owner,
        nodes: batch_nodes,
        cancel_scope: prepared.cancel_scope,
        producer: prepared.producer,
    });

    for seed in prepared.seeds {
        queue.push_back(Event::TokenEmitted(Token::seeded(
            seed.edge,
            seed.generation,
            seed.payload,
        )));
    }
}

// ── Remapping helpers ─────────────────────────────────────────────────────

/// Append a synthesized expression to the batch's segment and return its live id.
fn push_expr(exprs: &mut Vec<Expr>, expr_base: u32, expr: Expr) -> ExprId {
    let id = ExprId::new(expr_base + exprs.len() as u32);
    exprs.push(expr);
    id
}

/// `nodes.<name>.status != null`: the record exists. What a final `depends_on`
/// reference lowers to — satisfied by the record that made it final, kept in
/// the graph so the dependency is visible and replayable.
fn record_exists_guard(exprs: &mut Vec<Expr>, expr_base: u32, name: &SmolStr) -> ExprId {
    let nodes = push_expr(exprs, expr_base, Expr::Var(SmolStr::new("nodes")));
    let record = push_expr(exprs, expr_base, Expr::Field(nodes, name.clone()));
    let status = push_expr(
        exprs,
        expr_base,
        Expr::Field(record, SmolStr::new("status")),
    );
    let null = push_expr(exprs, expr_base, Expr::Lit(Value::Null));
    push_expr(exprs, expr_base, Expr::Binary(BinOp::Ne, status, null))
}

/// Conjoin a guard onto a node's precondition.
fn conjoin_precondition(node: &mut Node, exprs: &mut Vec<Expr>, expr_base: u32, guard: ExprId) {
    node.precondition = Some(match node.precondition {
        None => guard,
        Some(existing) => push_expr(exprs, expr_base, Expr::Binary(BinOp::And, guard, existing)),
    });
}

/// Shift one fragment expression into the live table at `offset`.
fn shift_expr(expr: &Expr<Local>, offset: u32) -> Expr {
    let id = |e: ExprId<Local>| ExprId::new(e.raw() + offset);
    match expr {
        Expr::Lit(v) => Expr::Lit(v.clone()),
        Expr::Var(name) => Expr::Var(name.clone()),
        Expr::Field(base, field) => Expr::Field(id(*base), field.clone()),
        Expr::Index(base, index) => Expr::Index(id(*base), id(*index)),
        Expr::Unary(op, arg) => Expr::Unary(*op, id(*arg)),
        Expr::Binary(op, lhs, rhs) => Expr::Binary(*op, id(*lhs), id(*rhs)),
        Expr::Cond {
            cond,
            then,
            otherwise,
        } => Expr::Cond {
            cond: id(*cond),
            then: id(*then),
            otherwise: id(*otherwise),
        },
        Expr::Array(items) => Expr::Array(items.iter().map(|i| id(*i)).collect()),
        Expr::Object(fields) => {
            Expr::Object(fields.iter().map(|(k, v)| (k.clone(), id(*v))).collect())
        }
        Expr::Call(name, args) => Expr::Call(name.clone(), args.iter().map(|a| id(*a)).collect()),
    }
}

fn remap_scope(scope: &Scope<Local>, scope_base: u32, expr_base: u32) -> Scope {
    let shift_env = |env: &std::collections::BTreeMap<SmolStr, ExprOrValue<Local>>| {
        env.iter()
            .map(|(key, value)| {
                let value = match value {
                    ExprOrValue::Value(v) => ExprOrValue::Value(v.clone()),
                    ExprOrValue::Expr(id) => ExprOrValue::Expr(ExprId::new(id.raw() + expr_base)),
                };
                (key.clone(), value)
            })
            .collect()
    };
    // Exhaustive destructuring: a new `Scope` or `ServiceSpec` field fails to
    // compile here instead of silently taking its default in every spliced scope.
    let Scope {
        id,
        env,
        runtime,
        workspace,
        services,
    } = scope;
    let mut out = Scope::new(ScopeId::new(scope_base + id.raw()));
    out.env = shift_env(env);
    out.runtime = runtime.clone();
    out.workspace = *workspace;
    out.services = services
        .iter()
        .map(|service| {
            let ir::ServiceSpec {
                name,
                image,
                env,
                ports,
                options,
                credentials,
            } = service;
            let mut s = ir::ServiceSpec::new(name, image);
            s.env = shift_env(env);
            s.ports = ports.clone();
            s.options = options.clone();
            s.credentials = credentials.clone();
            s
        })
        .collect();
    out
}

fn remap_node(
    source: &Node<Local>,
    node_base: u32,
    scope_base: u32,
    expr_base: u32,
    alloc: &mut Allocators,
) -> Node {
    let shift = |id: ExprId<Local>| ExprId::new(id.raw() + expr_base);
    // Exhaustive destructuring: a new `Node` field fails to compile here instead
    // of silently taking its default in every spliced node.
    let Node {
        id,
        name,
        scope,
        step,
        join,
        precondition,
        routing,
        budget,
        retry,
        run_on_cancel,
        tolerates_failure,
        splice_policy,
        meta,
        // Stays `None`: fragments are executable IR, validated as such.
        expand: _,
    } = source;
    let mut node = Node::new(
        NodeId::new(node_base + id.raw()),
        name,
        ScopeId::new(scope_base + scope.raw()),
        StepRef::new(
            step.kind.clone(),
            ir::placeholder::map_expr_ids(&step.config, &|id| id + u64::from(expr_base)),
        ),
    );
    node.join = *join;
    node.precondition = precondition.map(shift);
    node.budget = *budget;
    node.retry = retry.clone();
    node.run_on_cancel = *run_on_cancel;
    node.tolerates_failure = *tolerates_failure;
    node.splice_policy = *splice_policy;
    node.meta = meta.clone();
    for group in &routing.groups {
        let arms = group
            .arms
            .iter()
            .map(|arm| Edge {
                id: alloc.take_edge(),
                to: NodeId::new(node_base + arm.to.raw()),
                guard: match arm.guard {
                    Guard::Always => Guard::Always,
                    Guard::Expr(id) => Guard::Expr(shift(id)),
                },
                map: arm.map.map(shift),
                back: arm.back,
            })
            .collect();
        node.routing.groups.push(SelectGroup {
            arms,
            fallthrough: group.fallthrough,
        });
    }
    node
}
