# Engine IR Design: Token-Flow Graph with Explicit Routing

**Status:** Draft for v1, incorporating Core Semantics Patch 01 (`core-gaps-handoff.md`)
· **Scope:** Core IR, routing semantics, engine interface

## 1. Overview

The engine executes a directed graph (cycles permitted) using token-flow semantics.
A node fires when its **join policy** is satisfied by incoming tokens; on completion,
its **routing policy** emits tokens on outgoing edges. All coordination state lives in
a pure, sans-IO state machine (`engine` crate); side effects happen behind traits.

**Design rule for routing:** the default is *selection* — a completing node routes to
exactly one successor. *Fan-out is never implicit*; it must be explicitly represented
in the IR. This keeps single-path workflows analyzable and makes parallelism visible.

## 2. Routing: AND-of-XOR normal form

A node's routing is a list of **select groups**. Semantics:

- Each group independently emits **at most one** token: arms are evaluated in order,
  first guard that passes wins (XOR-select).
- Groups are independent and concurrent: **N groups may emit N tokens** (AND-split).

This one structure covers all cases explicitly:

| Pattern              | Representation                                      |
|----------------------|-----------------------------------------------------|
| Default: pick one    | 1 group, N guarded arms (+ fallthrough)             |
| Unconditional next   | 1 group, 1 arm, `Guard::Always`                     |
| Fan-out (parallel)   | N groups of 1 arm each                              |
| Conditional fan-out  | N groups; a group with no matching arm emits nothing (OR-split) |
| Loop back-edge       | An arm whose edge is marked `back` (bumps generation) |

Because fan-out requires *writing multiple groups*, it can never occur by accident.
Frontends and the native format enforce this: a plain `next:` produces one group; a
`parallel:` block produces multiple groups.

## 3. Core types (`ir` crate)

```rust
// ── Identifiers (newtypes over u32/u64; FiringId unique per run) ──────────
pub struct NodeId(u32);
pub struct EdgeId(u32);
pub struct ScopeId(u32);
pub struct ExprId(u32);       // index into the graph's expression table
pub struct FiringId(u64);
pub struct Generation(u32);   // loop-iteration counter carried by tokens
pub struct Attempt(u32);      // 1-based try counter; a retry is NOT a loop iteration

pub type Value = serde_json::Value;

// ── Guards & edges ────────────────────────────────────────────────────────
pub enum Guard {
    Always,
    /// Boolean expression over (outcome, contexts). Evaluated in the pure core.
    Expr(ExprId),
}

pub struct Edge {
    pub id: EdgeId,
    pub to: NodeId,
    pub guard: Guard,
    /// Payload for the emitted token; default = source outcome.output.
    pub map: Option<ExprId>,
    /// Back-edge: traversal increments the token's Generation.
    /// Validation: every cycle must contain >= 1 back edge.
    pub back: bool,
}

// ── Routing: AND of XORs ──────────────────────────────────────────────────
pub struct SelectGroup {
    /// Ordered; first arm whose guard passes wins.
    pub arms: Vec<Edge>,
    pub fallthrough: Fallthrough,
}

pub enum Fallthrough {
    /// No arm matched -> emit nothing (enables OR-split / loop exit).
    NoEmit,
    /// No arm matched -> run error (for frontends requiring totality).
    Error,
}

pub struct Routing {
    /// len == 1 -> pure selection (the default).
    /// len > 1  -> explicit fan-out; groups emit concurrently.
    pub groups: Vec<SelectGroup>,
}

// ── Joins ─────────────────────────────────────────────────────────────────
/// Tokens are matched per (node, generation).
pub enum JoinPolicy {
    /// A token present on every incoming edge (of the same generation).
    All,
    /// First token fires the node; later same-generation tokens are dropped.
    Any,
    /// Tokens on n distinct incoming edges (same generation).
    Quorum { n: u32 },
}

// ── Nodes & scopes ────────────────────────────────────────────────────────
pub struct Node {
    pub id: NodeId,
    pub name: SmolStr,
    pub scope: ScopeId,
    pub step: StepRef,
    pub join: JoinPolicy,
    /// GHA `if:`-style precondition, evaluated in the node's own context.
    /// False -> node completes with Status::Skipped without executing;
    /// routing still runs (so `always()`/`failure()` guards work downstream).
    pub precondition: Option<ExprId>,
    pub routing: Routing,
    pub budget: Budget,
    /// How many attempts this node gets. Default: one, no retries.
    pub retry: RetryPolicy,
    /// HIR only; lowered away before execution (see §6).
    pub expand: Option<Expansion>,
}

pub struct StepRef {
    pub kind: StepKindId,      // key into StepRegistry
    pub config: Value,         // may embed ExprId placeholders in HIR
}

pub struct Budget {
    pub max_firings: u32,      // hard cap per node across generations
    pub timeout: Duration,
}

/// Parallel for_each / matrix. Sequential for_each is NOT an Expansion —
/// it desugars to a cycle (see §6a). The engine has no "loop" primitive.
pub enum Expansion {
    ForEach {
        /// Evaluates to an array at runtime; one clone per element, with
        /// `item` and `index` bound into the clone's expression context.
        items: ExprId,
        target: ExpandTarget,
        /// Scheduler admission control across the spliced clones.
        max_parallel: Option<u32>,
        /// First clone failure cancels sibling clones (via the splice's
        /// CancelScope, §5a).
        fail_fast: bool,
    },
}

pub enum ExpandTarget {
    /// Clone this node only.
    Node,
    /// Clone the subgraph between entry and exit (loop bodies, matrix jobs).
    Subgraph { entry: NodeId, exit: NodeId },
}

/// "Job" generalized: a resource scope, not a sequence.
///
/// A scope is acquired before the first step in it starts and released once no live
/// firing, pending token or deferred join needs it. **Release is irreversible.** A
/// routing path or a cycle may leave a scope and come back; re-entry acquires a
/// fresh runtime and workspace, so anything the earlier firings left behind is gone.
/// Validation warns (it does not error) when any path can re-enter a scope — see §7.
///
/// The rejected alternative, for the record: holding a re-enterable scope open until
/// quiescence. That pins containers open on paths that may never execute.
pub struct Scope {
    pub id: ScopeId,
    pub env: BTreeMap<SmolStr, ExprOrValue>,
    pub runtime: RuntimeSpec,
    pub workspace: WorkspacePolicy,    // Shared | PerNode
}

pub struct RuntimeSpec {
    pub target: RuntimeTarget,         // HostProcess | Docker { image, .. }
    /// Opaque placement labels: GHA `runs-on`, BuildKite agent tags. The core never
    /// reads them. The v1 local executor maps the labels it knows and rejects
    /// unknown ones per label. No matching rules or queues until there is a
    /// distributed agent system to consume them.
    pub requirements: Vec<SmolStr>,
}

pub struct Graph {
    pub nodes: Vec<Node>,
    pub scopes: Vec<Scope>,
    pub exprs: ExprTable,
    pub entry: Vec<NodeId>,    // seeded with one Generation(0) token each
}
```

## 4. Runtime types

```rust
pub struct Token {
    pub edge: EdgeId,
    pub generation: Generation,   // `gen` is a reserved keyword in edition 2024
    pub payload: Value,
    pub from: FiringId,
}

pub struct Outcome {
    pub status: Status,
    /// Structured output — the value guards and `map` expressions see.
    pub output: Value,
    pub metrics: Metrics,
    /// The only write path into RunContext.kv. Merged in apply(), in event order.
    pub context_updates: BTreeMap<SmolStr, Value>,
}

/// THIS ENUM IS CLOSED. These six variants are the complete and permanent status
/// vocabulary; any future frontend concept maps onto them and never extends them.
pub enum Status {
    Success,
    /// Soft failure or partial completion. Routing-visible, and success-like.
    /// `underlying` carries the real failure whenever one was converted into this,
    /// so the log never records a clean success for something that failed.
    PartialSuccess { underlying: Option<FailureInfo> },
    Failure(FailureInfo),
    Skipped,
    Cancelled,
    TimedOut,
}

pub struct FailureInfo {
    pub message: String,
    /// What kind of failure this is, for `retry_on` to match: "network",
    /// "rate_limit", "exit_status:2", "retry_requested". Step kinds set it.
    pub class: SmolStr,
}
```

**One classification point.** `Status::is_success_like()` — `Success | PartialSuccess`
— is the *only* definition of success-likeness. Joins, cancel scopes, default success
guards and retry all call it; none of them open-codes the match. A guard that needs to
tell the two apart calls `partial_success()` or `full_success()`.

Three production paths yield `PartialSuccess`, and there is no node-level policy:
a step kind's config (`soft_fail`), retry exhaustion under
`Exhaustion::AcceptPartial`, and a step kind returning it directly.

### Run context

Run-scoped state that expressions read, and the only channel for one node to see
another's result. Nothing is threaded through token payloads for that purpose.

```rust
pub struct RunContext {
    /// Written ONLY by the core, per completed firing, keyed by node instance name
    /// (a matrix clone records under `build#2`).
    pub nodes: BTreeMap<SmolStr, NodeRecord>,
    /// Written ONLY via Outcome.context_updates, merged in apply().
    pub kv: BTreeMap<SmolStr, Value>,
}

pub struct NodeRecord {
    pub status: Status,          // final attempt, raw
    pub output: Value,
    pub generation: Generation,  // latest generation to complete
    pub attempts: u32,
}
```

**Write rule.** Every write happens inside `apply`, in event order: node records when
a firing's final attempt finishes, then `kv` merged last-write-wins in that same
order. No other write path exists. This is what keeps the core pure and replay
byte-identical. `RunContext` is derived state — reconstructible from the event log,
never checkpointed separately.

### Expression environment

```rust
pub struct EvalEnv<'a> {
    pub token: &'a Value,        // the payload on the first input edge
    pub run: &'a RunContext,     // exposed as `nodes.<id>.*` and `kv.*`
    pub statics: &'a StaticCtx,  // scope env, node identity, generation, attempt,
                                 // the firing's own outcome, `item` / `index`
}
```

Guards, `map`, preconditions and `Expansion.items` all take an `EvalEnv`. `token` and
`input` resolve to the token and shadow any static of the same name.

**Firing rule.** For each (node, generation): collect tokens per `JoinPolicy`; when
satisfied and budget allows, evaluate `precondition`, then either emit
`Command::StartStep` or synthesize a `Skipped` outcome. On outcome, evaluate
`routing`: token generation = source generation, `+1` per `back` edge.

**Attempts.** Each firing starts at `Attempt(1)`. When an attempt's outcome matches
`retry_on` and attempts remain, the core emits `Command::ScheduleRetry` and does
**not** run routing. Routing, the run-context record, and cancel-scope failure
propagation all key off the **final attempt only**; intermediate attempts live in the
event log and in `nodes.<id>.attempts`. A firing waiting out its backoff stays live,
so its scope stays held and the run is not quiescent. A cancelled firing is never
retried.

**Completion (quiescence).** The run is complete when there are no live firings and
no pending tokens can satisfy any join. Run status is folded from node outcomes.

## 5. Engine interface (pure core)

```rust
pub enum Event {
    RunStarted,
    TokenEmitted(Token),
    StepStarted   { firing: FiringId, attempt: Attempt },
    StepProgress  { firing: FiringId, ev: StepEvent },     // logs, artifacts, custom
    StepFinished  { firing: FiringId, attempt: Attempt, outcome: Outcome },
    /// The driver waited out a retry's backoff. Same pattern as timeouts: the
    /// driver applies jitter and does the sleeping, the core never sees a clock.
    RetryElapsed  { firing: FiringId, next_attempt: Attempt },
    NodeExpanded  { node: NodeId, splice: SubgraphSplice }, // for_each results
    /// External cancellation; the run's root scope cancels everything.
    CancelRequested { scope: CancelScopeId },
}

pub enum Command {
    /// Everything the executor needs, with every expression already resolved.
    /// `ResolvedFiring`'s constructor rejects a config that still holds an
    /// expression placeholder, so "no unresolved ExprId crosses the executor
    /// boundary" is an invariant of the type. Deserialization runs the same check.
    StartStep(ResolvedFiring),
    DeliverControl { firing: FiringId, ctl: Control },
    /// Wait out `base_delay`, then feed back RetryElapsed. The delay is
    /// `initial * factor^(n-1)` capped at `max`, computed by repeated multiplication
    /// so it is bit-identical on replay.
    ScheduleRetry  { firing: FiringId, next_attempt: Attempt, base_delay: Duration },
    ExpandNode     { node: NodeId, gen: Generation, expr: ExprId },
    AcquireScope   { scope: ScopeId },
    ReleaseScope   { scope: ScopeId },
    FinishRun      { status: RunStatus },
}

pub struct CancelScopeId(u32);
```

### 5a. Cancel scopes

A **CancelScope** is a dynamic set of firings that can be cancelled as a unit.
Every firing belongs to zero or more cancel scopes; scopes nest. Cancelling a
scope makes the core emit `DeliverControl { ctl: Control::Cancel }` for its live
firings and drop its pending tokens.

Sources of cancel scopes: the run itself (root scope), each `Expansion` splice
(this is what `fail_fast` triggers), and job-level cancel-on-failure. v2's
`Pause`/`Approve` reuse the same mechanism. Cancellation is a state transition
in the pure core; only signal delivery happens in executors.

```rust

/// Deterministic; no IO, no clocks, no randomness. Timeouts arrive as Events.
pub fn apply(state: EngineState, ev: Event) -> (EngineState, Vec<Command>);
```

Every `Event` is appended to a versioned event log before `apply`, including the ones
the core emits itself while routing. Each record carries its provenance —
`External` for what a host fed in, `Core` for what the core produced.

**Log version 2.** v1 → v2: the firing key gained `Attempt`, `StepStarted` /
`StepFinished` carry it, `ScheduleRetry` / `RetryElapsed` joined the vocabulary,
finish records carry `context_updates`, and every record records its provenance. A v1
log is rejected on read rather than half-understood.

**Replay.** Feeding a log's `External` records back through `apply` from a fresh state
reproduces the run. Everything marked `Core` is produced again rather than replayed,
which is what makes a byte-identical replayed log a determinism check: if any core
decision depended on a clock, on iteration order, or on anything outside the state,
the two logs diverge. Resume is still future work.

## 6. HIR -> Plan lowering

HIR is the same `Graph` shape with two relaxations: `StepRef.config` and guards may
contain unresolved expressions, and nodes may carry `expand`. Lowering is **lazy and
per-node**: when a node's join is satisfied, bind expressions against live contexts;
if `expand` is present, evaluate `items`, clone the target per element (binding
`item`/`index`), splice via `NodeExpanded`, and re-run the firing check on the clones.

**Splice semantics.** Each splice creates a fresh `CancelScope` over its clones and
adds one edge per clone into the downstream **collector** node. No special join is
needed: `JoinPolicy::All` counts incoming edges *as of firing time*, so dynamically
added edges are included. Each clone's output token carries its `index`; the
collector's `map` expression assembles the ordered result array.

A splice also **supersedes the whole template region** — every node the clones
replace, entry and interior alike. A superseded node never fires, and its outgoing
edges stop counting toward downstream joins.

This follows directly from `All` being defined over "incoming edges at firing time".
The template's own edge into the collector survives the splice, and no token can ever
cross it, because the node it leaves has been replaced. Counting it would make the
collector wait forever: a template edge left in the join count is a deadlock
generator. Superseding is not deletion — the edges stay in the graph so the log still
describes what was there — it only removes them from the count.

### 6a. Sequential for_each: desugars to a cycle

Iterating items one at a time (e.g. ordered region deploys) uses **no new IR** — the
frontend desugars it onto back-edges and generations:

- Entry node emits `{ items, idx: 0, acc: [] }`.
- Body chain executes against `items[idx]`.
- Final node has one select group with two arms:
  1. `back` edge, guard `idx + 1 < len(items)`, `map` = `{ idx: idx + 1, acc: acc ++ [output] }`
  2. exit edge (`Always`), payload = `acc ++ [output]`
- Generations distinguish iterations in logs/joins; `Budget.max_firings` caps the loop.

**Outcome-driven splice is deferred**, by decision. The splice *mechanism* ships
anyway via `Expansion::ForEach` — `NodeExpanded`, cancel scopes, instance namespacing
— so an outcome-driven entry point later is a new way into an existing path, not new
machinery. The fields it would add (`Outcome.splice`, `Node.allow_splice`) are
optional and additive: a backward-compatible change, not another format bump.

A hierarchical alternative — a `SubgraphStep` running a nested graph inside one step —
is deliberately rejected: it hides a second scheduler inside a step, so iterations
vanish from the event log and budgets/cancellation can't reach inside. Loops always
flatten into the one graph; only surface syntax is hierarchical.

### GHA frontend mapping

| GHA construct            | IR                                                        |
|--------------------------|-----------------------------------------------------------|
| job                      | `Scope` + chain of step nodes                             |
| steps (sequential)       | single-group `Always` edges between step nodes            |
| `needs: [a, b]`          | `JoinPolicy::All` on the dependent's entry node           |
| job with k dependents    | **k groups of 1 arm** on its exit node (explicit fan-out) |
| `if:`                    | `precondition` (Skipped propagates)                       |
| `strategy.matrix`        | `Expansion::ForEach { target: Subgraph }`; `fail-fast`/`max-parallel` map directly |
| composite action         | inlined subgraph                                          |

GHA lowering never produces back edges, `Any`/`Quorum` joins, or multi-arm groups —
it exercises the degenerate subset, by construction.

### Native format mapping

```yaml
- id: deploy-all
  for_each: ${{ outputs.plan.regions }}
  parallel: true      # true  -> Expansion::ForEach (splice + collector)
  body: [...]         # false -> cycle desugar (§6a)
```

## 7. Validation invariants (checked at load)

1. Every cycle contains at least one `back` edge.
2. `Guard::Always` may only appear as a group's final arm.
3. Groups are non-empty; `Routing.groups` may be empty (terminal node).
4. `Budget.max_firings >= 1`; any node reachable via a back edge must have a finite budget.
5. All `EdgeId`s unique; joins count distinct incoming edges.
6. Expression references resolve; HIR-only fields absent from executable plans.
7. `ExpandTarget::Subgraph { entry, exit }`: exit postdominates entry; no edges cross
   the subgraph boundary except into `entry` and out of `exit`.
8. Any node with an incoming back edge must use `JoinPolicy::Any`. A forward edge into
   the head carries only generation 0 and a back edge only generations 1 and up;
   tokens are matched per `(node, generation)`, so no generation ever holds a token on
   both and `All` is unsatisfiable forever.

   *Corollary users meet first:* a node cannot be both a multi-branch `All` join and a
   loop head. Put a dedicated join node in front of the loop head and let the back
   edge target the head.

   `Quorum { n: 1 }` behaves identically to `Any` today, but the invariant admits only
   `Any`: one canonical spelling is easier to grep and to review, and the equivalence
   is a property of the current join semantics rather than a guarantee worth making
   load-bearing. A frontend that naturally produces `Quorum { n: 1 }` runs
   `lower::normalize_loop_heads` instead.

**Warnings** (reported alongside errors; they do not block a load):

- A path can leave a scope and return to it. Release is irreversible (§3), so re-entry
  gets a fresh runtime and workspace. The warning names the re-entry node.

  **Suppressed** when the re-entry node joins with `All` and has at least one incoming
  forward edge from inside the scope. Such a node needs the inside edge's token to
  fire, and there are only two cases: either that token is emitted while the scope is
  still held, in which case it pins the scope until the join resolves and release
  cannot precede re-entry; or the inside arm never emits, in which case the `All` join
  is permanently unsatisfiable and the node never fires at all. Both are safe, and the
  test is purely structural. `Any` and `Quorum` re-entry nodes keep the warning: they
  can fire on the outside token alone, after release.

  What remains is still a static over-approximation — it cannot tell whether a given
  run reaches the releasing state.

## 8. Reserved seams (v2, no rework required)

- `StepKind::fingerprint(..) -> Option<Digest>` — defaults to `None`; content caching later.
- `#[non_exhaustive] enum Control { Cancel }` — Pause/Steer/Approve later.
- `Command::{Acquire,Release}Scope` — remote scope placement when distribution lands.
- `RuntimeSpec.requirements` — opaque placement labels now, matching semantics in v2.
- Cross-run concurrency groups live in the multi-run driver layer, later. No IR field
  now: v1 frontends **reject** GHA `concurrency:` and BuildKite `concurrency_group`
  with an unsupported-feature message rather than parse-and-ignore a mutual-exclusion
  feature. The eventual `concurrency_key: Option<ExprId>` is additive.
- Event log format is versioned from day one. Replay has landed (§5); resume and UI
  are still future work.
- Expression evaluation is **total**: a missing field is `null`, so a guard always
  yields a boolean. A typo is therefore silently falsy. Reserved for v2: a strict mode,
  or an unknown-field lint at load time that reports a path no context can bind.
- Firing contexts are built ad hoc today. `RunContext` / `EvalEnv` (handoff §2)
  replaces that mechanism outright when it lands — one way for expressions to see
  upstream state, not two.
