# Engine IR

A Rust implementation of [`ir-design.md`](ir-design.md): a token-flow graph with
explicit routing, plus the pure state machine that executes it.

```
crates/ir       core types, expressions, lowering helpers, load-time validation
crates/engine   the sans-IO state machine: apply(state, event) -> (state, commands)
```

A node fires when its **join policy** is satisfied by incoming tokens. On completion
its **routing policy** emits tokens on outgoing edges. Routing is an AND of XORs:
each select group emits at most one token, and groups emit concurrently. The default
is selection, so fan-out is never implicit — it takes writing more than one group.

```rust
use engine::{EngineState, Event, apply};

let mut state = EngineState::new(graph);
let (state, commands) = apply(state, Event::RunStarted);
// run the commands, feed the results back as events
```

## Where the design lands in the code

| Design section | Code |
|---|---|
| §2 routing, AND-of-XOR | `ir::graph::{Routing, SelectGroup, Guard, Fallthrough}` |
| §3 core types | `ir::graph`, `ir::ids` |
| §3 expressions | `ir::expr` — `Expr`, `ExprTable`, `Context`, `eval` |
| §4 runtime types | `ir::runtime` — `Token`, `Outcome`, `Status`, `Metrics` |
| §4 firing rule | `engine::apply::try_fire` |
| §4 quiescence | `EngineState::is_quiescent`, `apply::finish_if_quiescent` |
| §5 engine interface | `engine::event` — `Event`, `Command`; `engine::apply::apply` |
| §5 event log | `engine::log` — versioned from day one |
| §5a cancel scopes | `engine::state::CancelScope`, `apply::on_cancel` |
| §6 HIR → plan lowering | `engine::context::resolve_config`, `apply::expand` |
| §6 splice semantics | `engine::event::SubgraphSplice`, `apply::on_node_expanded` |
| §6a sequential `for_each` | `ir::lower::sequential_for_each` — no new IR, just a cycle |
| GHA frontend mapping | `crates/engine/tests/gha.rs` |
| §7 validation invariants | `ir::validate` |
| §8 reserved seams | `StepKind::fingerprint`, `Control`, `Command::{Acquire,Release}Scope`, `EventLog::version` |

## Reading the tests

The test suites are the executable form of the design document.

```
crates/engine/tests/routing.rs       §2 selection, fan-out, OR-split, guards, preconditions
crates/engine/tests/joins.rs         §3 All / Any / Quorum, generations, entry seeding
crates/engine/tests/loops.rs         §6a sequential for_each, back edges, firing budgets
crates/engine/tests/expansion.rs     §6 parallel for_each, collectors, max_parallel, fail_fast
crates/engine/tests/cancellation.rs  §5a cancel scopes, nesting, signal delivery
crates/engine/tests/scopes.rs        §3 resource scopes, env, acquire/release
crates/engine/tests/gha.rs           the GHA mapping table, end to end
crates/engine/tests/event_log.rs     §5 logging, determinism, serde round-trip, §8 seams
crates/ir/tests/validation.rs        §7, invariant by invariant
crates/ir/tests/expressions.rs       the expression language
```

`cargo test` runs all of them.

## Decisions the design document left open

**The expression language.** §3 names `ExprId` and says guards are "boolean
expressions over (outcome, contexts)" without fixing the language. `ir::expr`
defines a small total one: literals, variable lookup, field and index access,
arithmetic and comparison, short-circuiting boolean operators, conditionals, array
and object construction, and a fixed function set. Missing fields evaluate to `null`
rather than erroring, so a guard is always total. Two list functions, `sort_by_key`
and `pluck`, exist so a collector can put clone results back in `items` order
without needing lambdas.

**Firing contexts.** Two per firing. The *firing context* is what a precondition and
a step config see: `input`, `inputs`, `env`, `outputs`, `upstream`, `generation`,
`node`, plus `item` and `index` inside an expansion clone. `status` there is folded
from the upstream firings, which is what makes `if: success()` behave the way GHA's
does. The *outcome context* adds `output`, `outcome` and the firing's own `status`,
and is what routing guards and `map` expressions see.

**HIR config placeholders.** A `StepRef.config` marks an unresolved expression with
`{"$expr": <id>}` (`ir::validate::EXPR_PLACEHOLDER_KEY`). Lowering is lazy: the core
resolves placeholders against the firing context and puts the result in
`Command::StartStep.config`.

## Where the code departs from the document

Each of these is a place the document's literal text did not survive contact with
the compiler or with a working execution path.

1. **`Token.gen` is `Token.generation`.** `gen` is a reserved keyword in Rust
   edition 2024.

2. **`Command::StartStep` carries `config`.** Config resolution is lazy per §6, so
   the graph's copy is still unresolved when the step runs. The host needs the
   resolved value, not the placeholder.

3. **`apply` keeps its signature but drains an internal queue.** Routing emits
   tokens by feeding `Event::TokenEmitted` back to itself. Each one is appended to
   the log before it is applied, so the log records token flow as well as external
   input, and the caller only ever feeds in events from outside.

4. **Entry nodes and clone entries get synthetic seed edges.** §3 seeds entry nodes
   with a token, but a token names an edge and an entry node has no incoming edge.
   The engine allocates one seed edge per entry, so the join rule needs no special
   case: `All` over a single seed edge is satisfied by the seed token.

5. **`Command::ExpandNode` is defined but not emitted.** `items` is a pure
   expression, so the core evaluates it and builds the splice itself, in the same
   `apply` call. The variant stays as the seam for a host that resolves items
   externally and feeds back `Event::NodeExpanded`.

6. **`SubgraphSplice` carries the original region.** Every node the clones replace is
   *superseded*: it never fires, and its outgoing edges stop counting toward
   downstream joins. Without this the collector's `All` join would also wait on the
   original region's edge, which no token ever crosses.

7. **`ExpandTarget::Subgraph { entry, .. }` requires `entry` to be the expanding
   node.** Otherwise it is ambiguous whether the expanding node runs before the
   region is cloned. Violations are a run error, not a panic.

8. **Resource scopes are held, not refcounted.** A scope is acquired before the first
   step in it starts and released once no live firing, pending token or deferred join
   needs it. A per-firing refcount would tear a job down and rebuild it between two
   consecutive steps of the same job.

9. **A loop head must use `JoinPolicy::Any`.** On the first iteration only the entry
   edge carries a token and on later ones only the back edge does, so `All` would
   never be satisfied. `ir::lower::sequential_for_each` sets this for you.

10. **An entry node may have a back edge pointing at it.** §7 implies entry nodes
    are seeded rather than joined, but a loop head that is also the graph entry is
    legitimate: the seed starts generation 0 and the back edge starts each later one.
    Validation rejects only *forward* edges into an entry.

11. **Budget exhaustion drops the tokens.** §7 requires a finite cap on looped nodes
    but does not say what happens at the cap. The firing is refused, the tokens are
    dropped, a `RunError::BudgetExceeded` is recorded, and the run folds to failed —
    so a runaway loop terminates rather than spinning.

## Not built

Replay and resume (§5), content caching (§8), remote scope placement (§8), and
`Control::{Pause, Steer, Approve}` are all v2 in the document. The seams they need
are in place: the log is versioned and round-trips through serde, `EngineState`
serializes whole, `StepKind::fingerprint` exists and defaults to `None`, and
`Control` is `#[non_exhaustive]`.

No YAML or GitHub Actions parser is included. `crates/engine/tests/gha.rs` builds the
mapping table's output directly, which is what pins the semantics; a parser that
produces the same graphs is a separate piece of work.
