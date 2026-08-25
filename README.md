# Engine IR

A Rust implementation of [`ir-design.md`](ir-design.md), including Core Semantics
Patch 01 ([`core-gaps-handoff.md`](core-gaps-handoff.md)): a token-flow graph with
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
| §5 event log | `engine::log` — v2, with per-record provenance |
| §5 replay | `engine::replay` — `verify_replay` is the determinism canary |
| §4 retries | `ir::RetryPolicy`, `engine::apply::on_retry_elapsed` |
| §4 run context | `ir::RunContext`, `engine::state::EngineState::record_outcome` |
| §4 expression environment | `ir::EvalEnv` — one way to see upstream state |
| §5a cancel scopes | `engine::state::CancelScope`, `apply::on_cancel` |
| §6 HIR → plan lowering | `engine::context::resolve_config`, `apply::expand` |
| §6 splice semantics | `engine::event::SubgraphSplice`, `apply::on_node_expanded` |
| §6a sequential `for_each` | `ir::lower::sequential_for_each` — no new IR, just a cycle |
| GHA frontend mapping | `crates/engine/tests/gha.rs` |
| §7 validation invariants | `ir::validate` — `check` for errors and warnings, `validate` for errors alone |
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
crates/engine/tests/retries.rs       handoff §1 attempts, backoff, exhaustion
crates/engine/tests/run_context.rs   handoff §2 nodes.* / kv.*, goal gates, merge order
crates/engine/tests/partial_success.rs  handoff §4 soft failure, is_success_like
crates/engine/tests/seeding.rs       seed edges for entry nodes and clone entries
crates/engine/tests/resolved_firing.rs  the executor boundary: no unresolved ExprId crosses it
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

Evaluation is total by design: a guard must always produce a boolean, so a run never
fails at the wrong moment over a typo. The cost is that a typo is silently falsy
instead. A strict mode, or an unknown-field lint at load time, is a v2 seam (§8).

**Expression environment.** One `EvalEnv` for guards, `map`, preconditions and
`Expansion.items` alike. `token` and `input` resolve to the payload on the first input
edge and shadow any static of the same name; `nodes.*` and `kv.*` read the run
context; everything else — scope `env`, node identity, generation, attempt, the
firing's own outcome, `item` / `index` in a clone — comes from `StaticCtx`.

The earlier ad-hoc firing-context mechanism is gone rather than coexisting: `outputs`
and `upstream` bindings no longer exist, and a precondition's `success()` folds the
upstream statuses it reads out of `RunContext` by looking up each input edge's source
node. Nothing is threaded through token payloads to carry status.

## Folded back into the design document

These began as implementation notes and are now rules in `ir-design.md`, because each
follows from something the document already said rather than from a choice the code
made:

- **A splice supersedes the whole template region** (§6, splice semantics). `All` is
  defined over incoming edges *at firing time*, so a template edge that no token can
  cross is a deadlock generator. Superseding is not deletion: the edges stay in the
  graph and only leave the join count. Regression test:
  `superseded_template_edges_do_not_deadlock_the_collector`.
- **Scope release is irreversible** (§3, and a §7 warning). A scope is held until no
  firing, token or deferred join needs it; a path that leaves and returns gets a fresh
  runtime and workspace. Validation warns rather than errors, because whether a given
  path really releases the scope depends on runtime state. The check is a static
  over-approximation.
- **A loop head must join with `Any`** (§7, invariant 8), with the corollary that a
  node cannot be both a multi-branch `All` join and a loop head.
- **`Token.generation`** (§4) and **`Command::StartStep(ResolvedFiring)`** (§5).

Core Semantics Patch 01 landed on top of those: attempts and `RetryPolicy` (§3, §4),
`PartialSuccess` with `is_success_like` as the single classification point (§4),
`RunContext` and `EvalEnv` (§4), event log v2 with replay (§5), and
`RuntimeSpec.requirements` (§3).

## Where the code departs from the documents

Each of these is a place the literal text did not survive contact with the compiler or
with a working execution path.

### From the IR design

1. **Entry nodes and clone entries get synthetic seed edges.** §3 seeds entry nodes
   with a token, but a token names an edge and an entry node has none. The engine
   allocates one seed edge per entry, above every declared edge id, so joins count it
   like any other incoming edge and the firing rule needs no special case: `All` over
   a single seed edge is satisfied by the seed token. Seed edges never appear in a
   `Routing` group and never collide with a declared id
   (`crates/engine/tests/seeding.rs`).

2. **`Command::ExpandNode` is defined but not emitted.** `items` is a pure
   expression, so the core evaluates it and builds the splice itself, in the same
   `apply` call. The variant is marked `// reserved: external expansion`.

3. **`ExpandTarget::Subgraph { entry, .. }` requires `entry` to be the expanding
   node.** Otherwise it is ambiguous whether the expanding node runs before the region
   is cloned. Violations are a run error, not a panic.

4. **An entry node may have a back edge pointing at it.** A loop head that is also the
   graph entry is legitimate: the seed starts generation 0 and the back edge starts
   each later one. Validation rejects only *forward* edges into an entry.

5. **Budget exhaustion drops the tokens.** §7 requires a finite cap on looped nodes
   but does not say what happens at the cap. The firing is refused, the tokens are
   dropped, a `RunError::BudgetExceeded` is recorded, and the run folds to failed.

6. **`apply` drains an internal event queue.** The signature is the document's, but
   routing emits tokens by feeding `Event::TokenEmitted` back to itself, each logged
   before it is applied.

### From Core Semantics Patch 01

7. **`RetryOn.statuses` is `Vec<StatusKind>`, not `SmallVec<Status>`.** `Status`
   carries payloads — `Failure(FailureInfo)`, `PartialSuccess { underlying }` — so a
   list of them cannot express "match this variant". `StatusKind` is the payload-free
   discriminant. It is not a second classification point: the never-retry-a-success
   rule still goes through `Status::is_success_like`. `Vec` rather than `SmallVec`
   avoids adding a dependency for a list that is almost always one or two entries.

8. **`RuntimeSpec` became a struct.** It was an enum, and an enum cannot gain a
   common `requirements` field. It is now `RuntimeSpec { target: RuntimeTarget,
   requirements: Vec<SmolStr> }`, with the old variants moved to `RuntimeTarget`.

9. **Log records carry `EventSource`.** §7 test 1 asks for byte-identical replay, but
   nothing said how replay tells a core-emitted event from a host-fed one — and
   replaying core events instead of regenerating them would make the check a copy
   rather than a determinism test. Each record now says which it is, and
   `verify_replay` feeds back only the `External` ones.

10. **`context_updates` merge on final attempts only.** §1.2 pins "run-context
    recording of the final status" to the final attempt but does not say what happens
    to `kv` on an attempt that is then retried. Merging a discarded attempt's writes
    would leave state from work that was thrown away, so the merge follows the same
    rule as the status record. **Worth a second opinion** — a step that wants to leave
    a breadcrumb on every attempt cannot, today.

11. **A retry re-resolves the step config.** The attempt after a backoff binds its
    config against the run context as it stands then, not as it stood before attempt
    one. Deterministic either way; this way a retry can see what changed.

12. **`Exhaustion::AcceptPartial` fires whenever a retryable status has no attempts
    left**, including `max_attempts: 1` where no retry was ever possible. Reading it
    the other way would make `allow_partial` silently inert unless retries were also
    configured.

13. **`base_delay` uses repeated multiplication, not `powi`.** `powi` is not
    guaranteed bit-identical across platforms, and the delay goes into the log.

14. **`Status::is_success()` and `FailureInfo.retryable` are gone.** The first was a
    near-duplicate of `is_success_like` sitting next to it in the same `impl` — 
    exactly the footgun the one-classification-point rule warns about. The second was
    a second retry signal competing with `RetryPolicy` and `FailureInfo.class`.

15. **A firing awaiting a retry stays live.** That is what keeps its scope held and
    the run non-quiescent across the backoff, rather than a run appearing to finish
    mid-retry.

16. **`NodeRecord.gen` is `generation`**, for the same keyword reason as `Token`.

17. **`StaticCtx` is a defined type.** §2.3 names `statics: &StaticCtx` without
    saying what it holds. It holds the per-firing bindings that are neither the token
    nor run state: scope `env`, node identity, generation, attempt, the firing's own
    outcome, and `item` / `index`.

## Not built

Deliberately out of scope, per the handoff: outcome-driven splice (§3, deferred —
`Expansion::ForEach` already ships the mechanism), cross-run concurrency groups (D2 —
frontends must reject `concurrency:` rather than ignore it), and placement *semantics*
for `RuntimeSpec.requirements` (D3 — the labels are carried, uninterpreted).

Still v2 in the design document: resume, content caching, remote scope placement, and
`Control::{Pause, Steer, Approve}`. The seams are in place — the log is versioned and
rejects v1 cleanly, `EngineState` serializes whole, `StepKind::fingerprint` defaults
to `None`, and `Control` is `#[non_exhaustive]`. Replay has landed;
`engine::verify_replay` is the determinism canary.

No YAML or GitHub Actions parser is included. `crates/engine/tests/gha.rs` builds the
mapping table's output directly, which is what pins the semantics; a parser that
produces the same graphs is a separate piece of work.
