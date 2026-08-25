# Engine IR Design: Token-Flow Graph with Explicit Routing

**Status:** Draft for v1 · **Scope:** Core IR, routing semantics, engine interface

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
pub struct Scope {
    pub id: ScopeId,
    pub env: BTreeMap<SmolStr, ExprOrValue>,
    pub runtime: RuntimeSpec,          // HostProcess | Docker { image, .. }
    pub workspace: WorkspacePolicy,    // Shared | PerNode
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
    pub gen: Generation,
    pub payload: Value,
    pub from: FiringId,
}

pub struct Outcome {
    pub status: Status,
    /// Structured output — the value guards and `map` expressions see.
    pub output: Value,
    pub metrics: Metrics,
}

pub enum Status { Success, Failure(FailureInfo), Skipped, Cancelled, TimedOut }
```

**Firing rule.** For each (node, generation): collect tokens per `JoinPolicy`; when
satisfied and budget allows, evaluate `precondition`, then either emit
`Command::StartStep` or synthesize a `Skipped` outcome. On outcome, evaluate
`routing`: token generation = source generation, `+1` per `back` edge.

**Completion (quiescence).** The run is complete when there are no live firings and
no pending tokens can satisfy any join. Run status is folded from node outcomes.

## 5. Engine interface (pure core)

```rust
pub enum Event {
    RunStarted,
    TokenEmitted(Token),
    StepStarted   { firing: FiringId },
    StepProgress  { firing: FiringId, ev: StepEvent },     // logs, artifacts, custom
    StepFinished  { firing: FiringId, outcome: Outcome },
    NodeExpanded  { node: NodeId, splice: SubgraphSplice }, // for_each results
    /// External cancellation; the run's root scope cancels everything.
    CancelRequested { scope: CancelScopeId },
}

pub enum Command {
    StartStep      { firing: FiringId, node: NodeId, gen: Generation,
                     inputs: Vec<Token>, scope: ScopeId },
    DeliverControl { firing: FiringId, ctl: Control },
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

Every `Event` is appended to a versioned event log before `apply` (event sourcing;
replay/resume reserved for v2).

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

### 6a. Sequential for_each: desugars to a cycle

Iterating items one at a time (e.g. ordered region deploys) uses **no new IR** — the
frontend desugars it onto back-edges and generations:

- Entry node emits `{ items, idx: 0, acc: [] }`.
- Body chain executes against `items[idx]`.
- Final node has one select group with two arms:
  1. `back` edge, guard `idx + 1 < len(items)`, `map` = `{ idx: idx + 1, acc: acc ++ [output] }`
  2. exit edge (`Always`), payload = `acc ++ [output]`
- Generations distinguish iterations in logs/joins; `Budget.max_firings` caps the loop.

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

## 8. Reserved seams (v2, no rework required)

- `StepKind::fingerprint(..) -> Option<Digest>` — defaults to `None`; content caching later.
- `#[non_exhaustive] enum Control { Cancel }` — Pause/Steer/Approve later.
- `Command::{Acquire,Release}Scope` — remote scope placement when distribution lands.
- Event log format is versioned from day one — resume/replay/UI later.
