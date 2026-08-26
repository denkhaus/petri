//! Coordination state. Everything the engine knows lives here; nothing else does.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ir::{
    Attempt, CancelScopeId, Completion, EdgeId, EvalError, FiringId, Generation, Graph, NodeId,
    NodeRecord, Outcome, RunContext, RunStatus, ScopeId, Status, Token, Value,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::log::EventLog;

/// A node execution attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Firing {
    pub id: FiringId,
    pub node: NodeId,
    pub generation: Generation,
    /// Which try is running, 1-based. A retry advances this and never touches
    /// `generation`.
    pub attempt: Attempt,
    /// Resource scope: where the step runs.
    pub scope: ScopeId,
    /// Innermost cancel scope the firing belongs to.
    pub cancel_scope: CancelScopeId,
    pub inputs: Vec<Token>,
    /// The host reported `StepStarted`.
    pub started: bool,
    /// A `ScheduleRetry` is out; the firing stays live until `RetryElapsed` arrives,
    /// which is what keeps its scope held and the run non-quiescent.
    pub awaiting_retry: bool,
    /// A `Control::Cancel` has been delivered; the outcome will not be routed.
    pub cancelling: bool,
}

/// What a firing produced, kept for status folding and for the `outputs` context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FiringRecord {
    pub firing: FiringId,
    pub node: NodeId,
    pub name: SmolStr,
    pub generation: Generation,
    /// The attempt this outcome came from. Only final attempts are recorded here.
    pub attempt: Attempt,
    pub outcome: Outcome,
}

/// A dynamic set of firings cancellable as a unit. Scopes nest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CancelScope {
    pub id: CancelScopeId,
    pub parent: Option<CancelScopeId>,
    pub children: Vec<CancelScopeId>,
    /// Nodes covered by this scope. The root scope covers everything and leaves
    /// this empty.
    pub nodes: BTreeSet<NodeId>,
    pub cancelled: bool,
    /// The forced tier: nothing in the scope fires or routes any more, and
    /// `run_on_cancel` admits nothing. Killed implies cancelled.
    #[serde(default)]
    pub killed: bool,
}

/// Bookkeeping for one spliced expansion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Splice {
    pub cancel_scope: CancelScopeId,
    pub source: NodeId,
    pub max_parallel: Option<u32>,
    pub fail_fast: bool,
    /// Every node the splice added, for admission control and cancellation.
    pub nodes: BTreeSet<NodeId>,
}

/// Anything that makes a run fail without a step failing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum RunError {
    #[error("node {node:?}: evaluating {site} failed: {error}")]
    Eval {
        node: NodeId,
        site: SmolStr,
        error: EvalError,
    },
    #[error("node {node:?} exceeded its firing budget of {max_firings}")]
    BudgetExceeded { node: NodeId, max_firings: u32 },
    #[error("node {node:?}: select group {group} matched no arm and requires one")]
    NoArmMatched { node: NodeId, group: usize },
    #[error("node {node:?}: `for_each` items evaluated to {got}, not an array")]
    ItemsNotArray { node: NodeId, got: SmolStr },
    #[error("node {node:?}: step config still holds an unresolved expression at `{path}`")]
    UnresolvedConfig { node: NodeId, path: String },
    #[error("firing {firing:?} is not waiting for a retry")]
    UnexpectedRetry { firing: FiringId },
    #[error("firing {firing:?} reported attempt {reported:?} while running {running:?}")]
    AttemptMismatch {
        firing: FiringId,
        reported: Attempt,
        running: Attempt,
    },
    #[error("node {node:?}: expansion subgraph entry must be the expanding node")]
    ExpansionEntryMismatch { node: NodeId },
    #[error("token refers to unknown edge {0:?}")]
    UnknownEdge(EdgeId),
    #[error("unknown node {0:?}")]
    UnknownNode(NodeId),
    #[error("unknown firing {0:?}")]
    UnknownFiring(FiringId),
    #[error("event arrived before RunStarted")]
    NotStarted,
    #[error("event arrived after the run finished")]
    AlreadyFinished,
}

/// The whole state machine's state. Pure data: no handles, no clocks, no IO.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineState {
    /// The live graph. Expansions splice clones into it, so it grows during a run.
    pub graph: Graph,
    /// Every event applied so far, in order.
    pub log: EventLog,

    /// Tokens waiting on a join: node, then generation, then edge. Nested rather
    /// than keyed by a `(node, generation)` tuple so the whole state serializes to
    /// JSON, where a map key has to be a primitive.
    pending: BTreeMap<NodeId, BTreeMap<Generation, BTreeMap<EdgeId, Token>>>,
    /// `(node, generation)` pairs that already fired. Later tokens for them are
    /// dropped, which is what makes `JoinPolicy::Any` fire exactly once.
    fired: BTreeSet<(NodeId, Generation)>,
    /// Joins that are satisfied but held back by `max_parallel`.
    deferred: VecDeque<(NodeId, Generation)>,

    live: BTreeMap<FiringId, Firing>,
    firing_counts: BTreeMap<NodeId, u32>,
    history: Vec<FiringRecord>,
    /// Firings whose latest recorded outcome is `Cancelled` — the admission check
    /// runs per token, so this is kept alongside `history` rather than scanned out
    /// of it.
    #[serde(default)]
    cancelled_outcomes: BTreeSet<FiringId>,
    /// Run-scoped state expressions read as `nodes.*` and `kv.*`.
    ///
    /// Derived: every write happens in `apply`, in event order, so replaying the log
    /// rebuilds it exactly. It is never checkpointed as a separate artifact.
    run: RunContext,

    cancel_scopes: BTreeMap<CancelScopeId, CancelScope>,
    /// Innermost cancel scope per node; anything unlisted belongs to the root.
    node_cancel_scope: BTreeMap<NodeId, CancelScopeId>,
    splices: Vec<Splice>,
    /// Nodes replaced by expansion clones. They never fire, and their outgoing
    /// edges stop counting toward downstream joins.
    superseded: BTreeSet<NodeId>,
    /// `item` / `index` bindings a clone's nodes see.
    clone_bindings: BTreeMap<NodeId, BTreeMap<SmolStr, Value>>,

    /// Firings settled by a cancel or kill while awaiting a retry backoff. The
    /// driver's sleeper cannot be recalled, so the one matching late `RetryElapsed`
    /// consumes its tombstone silently; any other invalid `RetryElapsed` still
    /// errors.
    #[serde(default)]
    retry_tombstones: BTreeSet<FiringId>,

    /// Synthetic incoming edges for entry nodes and clone entries.
    seed_edges: BTreeMap<EdgeId, NodeId>,
    /// Resource scopes currently held. A scope is held from the moment one of its
    /// nodes starts until nothing in it can run again, so a chain of steps in one
    /// job does not tear the job down between steps.
    held_scopes: BTreeSet<ScopeId>,

    next_firing: u64,
    next_cancel_scope: u32,
    next_edge: u32,

    started: bool,
    finished: bool,
    cancelled: bool,
    errors: Vec<RunError>,
}

impl EngineState {
    /// A state ready to receive `Event::RunStarted`.
    pub fn new(graph: Graph) -> Self {
        let next_edge = graph
            .edges()
            .map(|e| e.id.raw())
            .filter(|id| *id != EdgeId::SEED.raw())
            .max()
            .map_or(0, |m| m + 1);
        let mut cancel_scopes = BTreeMap::new();
        cancel_scopes.insert(
            CancelScopeId::ROOT,
            CancelScope {
                id: CancelScopeId::ROOT,
                parent: None,
                children: Vec::new(),
                nodes: BTreeSet::new(),
                cancelled: false,
                killed: false,
            },
        );
        Self {
            graph,
            log: EventLog::new(),
            pending: BTreeMap::new(),
            fired: BTreeSet::new(),
            deferred: VecDeque::new(),
            live: BTreeMap::new(),
            firing_counts: BTreeMap::new(),
            history: Vec::new(),
            cancelled_outcomes: BTreeSet::new(),
            run: RunContext::new(),
            cancel_scopes,
            node_cancel_scope: BTreeMap::new(),
            splices: Vec::new(),
            superseded: BTreeSet::new(),
            clone_bindings: BTreeMap::new(),
            retry_tombstones: BTreeSet::new(),
            seed_edges: BTreeMap::new(),
            held_scopes: BTreeSet::new(),
            next_firing: 1,
            next_cancel_scope: 1,
            next_edge,
            started: false,
            finished: false,
            cancelled: false,
            errors: Vec::new(),
        }
    }

    // ── Read-only views ────────────────────────────────────────────────────

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// The root scope was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn errors(&self) -> &[RunError] {
        &self.errors
    }

    pub fn history(&self) -> &[FiringRecord] {
        &self.history
    }

    pub fn live_firings(&self) -> impl Iterator<Item = &Firing> {
        self.live.values()
    }

    pub fn firing(&self, id: FiringId) -> Option<&Firing> {
        self.live.get(&id)
    }

    /// The node a firing belongs to, live or finished.
    ///
    /// [`EngineState::firing`] sees only live firings, and by the time an observer
    /// runs for a finish record the firing is already retired — so this searches
    /// history too. The stable way for a host to resolve a firing to its node, and
    /// from there its name and `meta`.
    pub fn firing_node(&self, id: FiringId) -> Option<NodeId> {
        self.live.get(&id).map(|f| f.node).or_else(|| {
            self.history
                .iter()
                .rev()
                .find(|r| r.firing == id)
                .map(|r| r.node)
        })
    }

    /// Run-scoped state as expressions see it.
    pub fn run_context(&self) -> &RunContext {
        &self.run
    }

    /// Latest output of a node instance, by name.
    pub fn output(&self, name: &str) -> Option<&Value> {
        self.run.node(name).map(|record| &record.output)
    }

    /// How many times a node has fired, across generations.
    pub fn firing_count(&self, node: NodeId) -> u32 {
        self.firing_counts.get(&node).copied().unwrap_or(0)
    }

    /// Tokens still waiting on a join, with the `(node, generation)` they wait at.
    pub fn pending_tokens(&self) -> impl Iterator<Item = ((NodeId, Generation), &Token)> {
        self.pending.iter().flat_map(|(node, generations)| {
            generations.iter().flat_map(move |(generation, tokens)| {
                tokens.values().map(move |t| ((*node, *generation), t))
            })
        })
    }

    pub fn pending_count(&self) -> usize {
        self.pending
            .values()
            .flat_map(|g| g.values())
            .map(|t| t.len())
            .sum()
    }

    /// The synthetic incoming edges allocated for entry nodes and expansion clone
    /// entries, with the node each one feeds.
    ///
    /// These are allocated from the free edge-id space above every id the graph
    /// declares, so they never collide with a declared edge, and they are never
    /// written into a [`ir::Routing`] group — they exist only here. Joins count them
    /// alongside real incoming edges, which is what lets an entry node use any join
    /// policy without a special case.
    pub fn seed_edges(&self) -> impl Iterator<Item = (EdgeId, NodeId)> + '_ {
        self.seed_edges.iter().map(|(edge, node)| (*edge, *node))
    }

    pub fn cancel_scope(&self, id: CancelScopeId) -> Option<&CancelScope> {
        self.cancel_scopes.get(&id)
    }

    pub fn splices(&self) -> &[Splice] {
        &self.splices
    }

    /// Nodes replaced by expansion clones.
    pub fn is_superseded(&self, node: NodeId) -> bool {
        self.superseded.contains(&node)
    }

    /// Nothing is running and nothing more can start.
    ///
    /// Joins are checked on every token arrival, so once no firing is live and
    /// nothing is deferred, no pending token can ever satisfy a join. A firing
    /// waiting out a retry backoff is still live, so a run mid-backoff is not
    /// quiescent.
    pub fn is_quiescent(&self) -> bool {
        self.live.is_empty() && self.deferred.is_empty()
    }

    /// Firings waiting out a retry backoff.
    pub fn awaiting_retry(&self) -> impl Iterator<Item = &Firing> {
        self.live.values().filter(|f| f.awaiting_retry)
    }

    /// The run status folded from node outcomes, as it stands right now, under the
    /// graph's [`Completion`] policy.
    pub fn folded_status(&self) -> RunStatus {
        if self.cancelled {
            return RunStatus::Cancelled;
        }
        // Engine errors are never control flow: they fail the run under both
        // policies.
        if !self.errors.is_empty() {
            return RunStatus::Failed;
        }
        match self.graph.completion {
            Completion::AnyFailure => {
                if self.any_failure() {
                    RunStatus::Failed
                } else {
                    RunStatus::Success
                }
            }
            // Success iff the terminal node has a success-like final record.
            // Failures elsewhere are control flow; a missing record means the run
            // never got there, which fails it whatever else succeeded.
            Completion::TerminalNode(id) => {
                let record = self
                    .graph
                    .node(id)
                    .and_then(|node| self.run.node(&node.name));
                match record {
                    Some(record) if record.status.is_success_like() => RunStatus::Success,
                    _ => RunStatus::Failed,
                }
            }
        }
    }

    /// Whether anything has failed so far: an engine error, or any failed record in
    /// history.
    ///
    /// This is what the `run.failed` static means — "any failure so far", under
    /// every completion policy. It is deliberately not [`Self::folded_status`]:
    /// under [`Completion::TerminalNode`] the fold reads `Failed` until the exit
    /// record exists, which would poison `run.failed` guards mid-run. Two names,
    /// two meanings.
    pub fn any_failure(&self) -> bool {
        !self.errors.is_empty() || self.history.iter().any(|r| r.outcome.status.is_failure())
    }

    // ── Mutation used by `apply` ───────────────────────────────────────────

    pub(crate) fn mark_started(&mut self) {
        self.started = true;
    }

    pub(crate) fn mark_finished(&mut self) {
        self.finished = true;
    }

    pub(crate) fn mark_cancelled(&mut self) {
        self.cancelled = true;
    }

    pub(crate) fn push_error(&mut self, error: RunError) {
        self.errors.push(error);
    }

    pub(crate) fn next_firing_id(&mut self) -> FiringId {
        let id = FiringId::new(self.next_firing);
        self.next_firing += 1;
        id
    }

    pub(crate) fn next_cancel_scope_id(&mut self) -> CancelScopeId {
        let id = CancelScopeId::new(self.next_cancel_scope);
        self.next_cancel_scope += 1;
        id
    }

    pub(crate) fn next_edge_id(&mut self) -> EdgeId {
        let id = EdgeId::new(self.next_edge);
        self.next_edge += 1;
        id
    }

    pub(crate) fn register_seed_edge(&mut self, edge: EdgeId, node: NodeId) {
        self.seed_edges.insert(edge, node);
    }

    /// Where a token on this edge is headed. Covers seed edges as well as real ones.
    pub(crate) fn edge_target(&self, edge: EdgeId) -> Option<NodeId> {
        self.graph
            .edge(edge)
            .map(|e| e.to)
            .or_else(|| self.seed_edges.get(&edge).copied())
    }

    /// Edges that count toward a node's join right now: its real incoming edges,
    /// minus those from superseded sources, plus any seed edge aimed at it.
    pub(crate) fn incoming_edges(&self, node: NodeId) -> Vec<EdgeId> {
        let mut ids: Vec<EdgeId> = self
            .graph
            .nodes
            .iter()
            .filter(|n| !self.superseded.contains(&n.id))
            .flat_map(|n| n.routing.edges())
            .filter(|e| e.to == node)
            .map(|e| e.id)
            .collect();
        ids.extend(
            self.seed_edges
                .iter()
                .filter(|(_, target)| **target == node)
                .map(|(edge, _)| *edge),
        );
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub(crate) fn has_fired(&self, key: (NodeId, Generation)) -> bool {
        self.fired.contains(&key)
    }

    pub(crate) fn mark_fired(&mut self, key: (NodeId, Generation)) {
        self.fired.insert(key);
    }

    pub(crate) fn store_token(&mut self, node: NodeId, token: Token) {
        self.pending
            .entry(node)
            .or_default()
            .entry(token.generation)
            .or_default()
            .insert(token.edge, token);
    }

    pub(crate) fn tokens_for(&self, key: (NodeId, Generation)) -> Option<&BTreeMap<EdgeId, Token>> {
        self.pending.get(&key.0).and_then(|g| g.get(&key.1))
    }

    pub(crate) fn take_tokens(&mut self, key: (NodeId, Generation)) -> Vec<Token> {
        let Some(generations) = self.pending.get_mut(&key.0) else {
            return Vec::new();
        };
        let taken = generations
            .remove(&key.1)
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        if generations.is_empty() {
            self.pending.remove(&key.0);
        }
        taken
    }

    /// Drop every pending token aimed at one of these nodes.
    pub(crate) fn drop_tokens_for_nodes(&mut self, nodes: &BTreeSet<NodeId>) {
        self.pending.retain(|node, _| !nodes.contains(node));
        self.deferred.retain(|(node, _)| !nodes.contains(node));
    }

    pub(crate) fn drop_all_tokens(&mut self) {
        self.pending.clear();
        self.deferred.clear();
    }

    pub(crate) fn bump_firing_count(&mut self, node: NodeId) {
        *self.firing_counts.entry(node).or_insert(0) += 1;
    }

    pub(crate) fn insert_firing(&mut self, firing: Firing) {
        self.live.insert(firing.id, firing);
    }

    pub(crate) fn firing_mut(&mut self, id: FiringId) -> Option<&mut Firing> {
        self.live.get_mut(&id)
    }

    pub(crate) fn remove_firing(&mut self, id: FiringId) -> Option<Firing> {
        self.live.remove(&id)
    }

    /// Record a firing's final outcome: history, the node's run-context record, and
    /// the `kv` merge, in that order.
    ///
    /// This is the only write path into [`RunContext`], and it runs inside `apply`,
    /// in event order. Intermediate retry attempts never reach it.
    pub(crate) fn record_outcome(&mut self, record: FiringRecord) {
        self.run.record(
            record.name.clone(),
            NodeRecord {
                status: record.outcome.status.clone(),
                output: record.outcome.output.clone(),
                generation: record.generation,
                attempts: record.attempt.raw(),
            },
        );
        self.run.merge(&record.outcome.context_updates);
        if matches!(record.outcome.status, Status::Cancelled) {
            self.cancelled_outcomes.insert(record.firing);
        } else {
            self.cancelled_outcomes.remove(&record.firing);
        }
        self.history.push(record);
    }

    pub(crate) fn clone_bindings_for(&self, node: NodeId) -> Option<&BTreeMap<SmolStr, Value>> {
        self.clone_bindings.get(&node)
    }

    pub(crate) fn set_clone_bindings(&mut self, node: NodeId, bindings: BTreeMap<SmolStr, Value>) {
        self.clone_bindings.insert(node, bindings);
    }

    pub(crate) fn supersede(&mut self, node: NodeId) {
        self.superseded.insert(node);
    }

    pub(crate) fn add_cancel_scope(
        &mut self,
        id: CancelScopeId,
        parent: CancelScopeId,
        nodes: BTreeSet<NodeId>,
    ) {
        self.cancel_scopes.insert(
            id,
            CancelScope {
                id,
                parent: Some(parent),
                children: Vec::new(),
                nodes,
                cancelled: false,
                killed: false,
            },
        );
        if let Some(p) = self.cancel_scopes.get_mut(&parent) {
            p.children.push(id);
        }
    }

    pub(crate) fn set_node_cancel_scope(&mut self, node: NodeId, scope: CancelScopeId) {
        self.node_cancel_scope.insert(node, scope);
    }

    pub(crate) fn cancel_scope_of(&self, node: NodeId) -> CancelScopeId {
        self.node_cancel_scope
            .get(&node)
            .copied()
            .unwrap_or(CancelScopeId::ROOT)
    }

    /// A scope and everything nested inside it.
    pub(crate) fn cancel_scope_closure(&self, root: CancelScopeId) -> BTreeSet<CancelScopeId> {
        let mut out = BTreeSet::new();
        let mut queue = VecDeque::from([root]);
        while let Some(id) = queue.pop_front() {
            if !out.insert(id) {
                continue;
            }
            if let Some(scope) = self.cancel_scopes.get(&id) {
                queue.extend(scope.children.iter().copied());
            }
        }
        out
    }

    pub(crate) fn mark_scope_cancelled(&mut self, id: CancelScopeId) {
        if let Some(scope) = self.cancel_scopes.get_mut(&id) {
            scope.cancelled = true;
        }
    }

    pub(crate) fn is_scope_cancelled(&self, id: CancelScopeId) -> bool {
        self.cancel_scopes
            .get(&id)
            .is_some_and(|scope| scope.cancelled)
    }

    /// Whether any scope on the node's chain, innermost to root, satisfies `pred`.
    fn any_enclosing_scope(&self, node: NodeId, pred: impl Fn(&CancelScope) -> bool) -> bool {
        let mut current = Some(self.cancel_scope_of(node));
        while let Some(id) = current {
            let Some(scope) = self.cancel_scopes.get(&id) else {
                return false;
            };
            if pred(scope) {
                return true;
            }
            current = scope.parent;
        }
        false
    }

    /// Whether the node sits in a cancelled scope.
    pub(crate) fn is_node_cancelled(&self, node: NodeId) -> bool {
        self.cancelled || self.any_enclosing_scope(node, |scope| scope.cancelled)
    }

    /// Mark a scope killed. Killed implies cancelled.
    pub(crate) fn mark_scope_killed(&mut self, id: CancelScopeId) {
        if let Some(scope) = self.cancel_scopes.get_mut(&id) {
            scope.cancelled = true;
            scope.killed = true;
        }
    }

    /// Whether the node sits in a killed scope. A root kill marks the root scope,
    /// and every node's scope chain ends there, so no separate run flag is needed.
    pub(crate) fn is_node_killed(&self, node: NodeId) -> bool {
        self.any_enclosing_scope(node, |scope| scope.killed)
    }

    /// Whether this firing's recorded final outcome is `Cancelled`. Seed tokens
    /// carry `FiringId(0)`, which no record ever uses.
    pub(crate) fn outcome_was_cancelled(&self, firing: FiringId) -> bool {
        self.cancelled_outcomes.contains(&firing)
    }

    pub(crate) fn add_retry_tombstone(&mut self, firing: FiringId) {
        self.retry_tombstones.insert(firing);
    }

    /// Consume the tombstone for a settled awaiting-retry firing, if one exists.
    pub(crate) fn take_retry_tombstone(&mut self, firing: FiringId) -> bool {
        self.retry_tombstones.remove(&firing)
    }

    pub(crate) fn nodes_in_scopes(&self, scopes: &BTreeSet<CancelScopeId>) -> BTreeSet<NodeId> {
        let mut nodes = BTreeSet::new();
        for id in scopes {
            if let Some(scope) = self.cancel_scopes.get(id) {
                nodes.extend(scope.nodes.iter().copied());
            }
        }
        nodes
    }

    pub(crate) fn push_splice(&mut self, splice: Splice) {
        self.splices.push(splice);
    }

    pub(crate) fn splice_for_node(&self, node: NodeId) -> Option<&Splice> {
        self.splices.iter().find(|s| s.nodes.contains(&node))
    }

    /// Live firings inside a splice, for `max_parallel` admission control.
    pub(crate) fn live_in_splice(&self, splice: &Splice) -> u32 {
        self.live
            .values()
            .filter(|f| splice.nodes.contains(&f.node))
            .count() as u32
    }

    pub(crate) fn defer(&mut self, key: (NodeId, Generation)) {
        if !self.deferred.contains(&key) {
            self.deferred.push_back(key);
        }
    }

    pub(crate) fn take_deferred(&mut self) -> Vec<(NodeId, Generation)> {
        self.deferred.drain(..).collect()
    }

    /// Mark a scope held. Returns true the first time, when the host must acquire it.
    pub(crate) fn acquire_scope(&mut self, scope: ScopeId) -> bool {
        self.held_scopes.insert(scope)
    }

    /// Scopes with a live firing, a pending token or a deferred join still in them.
    fn needed_scopes(&self) -> BTreeSet<ScopeId> {
        let mut needed: BTreeSet<ScopeId> = self.live.values().map(|f| f.scope).collect();
        let waiting = self
            .pending
            .iter()
            .filter(|(_, generations)| generations.values().any(|t| !t.is_empty()))
            .map(|(node, _)| *node)
            .chain(self.deferred.iter().map(|(node, _)| *node));
        for node in waiting {
            if let Some(nd) = self.graph.node(node) {
                needed.insert(nd.scope);
            }
        }
        needed
    }

    /// Drop the scopes nothing needs any more, and say which they were.
    pub(crate) fn release_unneeded_scopes(&mut self) -> Vec<ScopeId> {
        let needed = self.needed_scopes();
        let released: Vec<ScopeId> = self.held_scopes.difference(&needed).copied().collect();
        for scope in &released {
            self.held_scopes.remove(scope);
        }
        released
    }

    /// Terminal release: drop every held scope, and say which they were. Nothing can
    /// need an environment after `FinishRun`, and a finished serialized state must
    /// claim no resources — a token parked at an unsatisfiable join no longer holds
    /// its environment past the end of the run.
    pub(crate) fn release_all_scopes(&mut self) -> Vec<ScopeId> {
        let released: Vec<ScopeId> = self.held_scopes.iter().copied().collect();
        self.held_scopes.clear();
        released
    }

    /// Scopes the host is currently holding.
    pub fn held_scopes(&self) -> impl Iterator<Item = ScopeId> + '_ {
        self.held_scopes.iter().copied()
    }
}

/// A synthesized outcome for a node that never ran.
pub(crate) fn synthetic(status: Status) -> Outcome {
    Outcome::new(status, Value::Null)
}
