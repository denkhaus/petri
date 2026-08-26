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
| exec §1 layering | `executor::Executor` / `ExecEnv`, `steps::StepRunner`, `driver::Driver` |
| exec §2 driver loop | `driver::run` — one channel, so arrival order is the total order |
| exec §3 process step | `steps::process`, `steps::outputs` |
| exec §4 cancellation | `steps::process::ladder`, `driver::Driver::on_hard_deadline` |
| exec §5 environments | `executor::host`, `executor::docker`, `executor::scope` |
| exec §6 secrets | `executor::secrets`, `driver::LogSink` |
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

crates/driver/tests/e2e.rs           exec §7 1-2: native loop and GHA-shaped, real processes
crates/driver/tests/cancellation.rs  exec §7 3,4,5,9: the ladder and the hard deadline
crates/driver/tests/timeout.rs       exec §7 6: timeouts, and the race under replay
crates/driver/tests/environments.rs  exec §7 7,10: acquire failure and retention
crates/driver/tests/secrets.rs       exec §7 8: masking, and what reaches the log
crates/driver/tests/docker.rs        exec §7 3,10 Docker halves; skipped without a daemon
```

Docker tests skip with a message when no daemon is reachable, so `cargo test` is
green on a machine without one.

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

### From the executor handoff

18. **`Executor::release` takes a `ScopeOutcome`.** The handoff's signature is
    `release(&self, env: EnvHandle)`, but the retention default is *keep on failure*
    and an environment cannot know whether the work inside it failed. The driver
    knows, so it says.

19. **`ProcessHandle` exposes one merged `lines()` stream, not `stdout()` and
    `stderr()`.** §3.4 requires log lines in arrival order across both streams, and
    merging two receivers after the fact cannot recover an order that was never
    recorded. Each line carries its stream tag, so nothing is lost.

20. **The cancellation escalation is in `output`, not a failure class.** §2.4 asks
    for `Status::Cancelled` with class `"cancel_forced"`, and §7 test 5 for a class
    recording the TERM-to-KILL escalation — but `Status::Cancelled` carries no
    `FailureInfo`, and adding one would widen a closed enum that core §4 says is
    permanent. So `cancel_escalation` is a field on the outcome's output object,
    valued `sigterm`, `sigkill` or `cancel_forced`.

21a. **In-container signals are `kill -SIG -PGID`, with no `--` separator.** The
    handoff spells it `docker exec <c> kill -<SIG> -- -<PGID>`, which is the POSIX
    form — and busybox rejects it outright: `sh: invalid number '--'`. A rejected
    signal is a silent one, so cancellation never reached the step; the ladder waited
    out its whole grace period and the step ran on until the container was torn down.
    Since alpine is the obvious base image, the separator cannot be used.
    `kill -TERM -123` is understood by busybox ash, dash and bash alike, and the call
    goes through `sh -c` so it is the shell builtin rather than whichever `kill`
    binary the image carries.

    This one hid behind a green test. `docker_cancel_kills_the_exec_process_group`
    asserted that a backgrounded grandchild stopped ticking after a cancel — which it
    did, but because release removed the container, not because the signal landed.
    The test now also asserts the escalation was `sigterm`: that step does not trap
    TERM, so TERM alone must have ended it, and a signal that never arrives shows up
    as a `sigkill` escalation instead. **A cancellation test that only checks the
    process is gone will pass on teardown alone; it has to pin how it went.**

21b. **Docker `wait` follows the process group, not the `docker exec` client.** `setsid` forks when its caller is already a process-group
    leader, and whether `docker exec` hands it one is not something to rely on. When
    it forks, `setsid` exits as soon as the child is running and `docker exec` returns
    0 while the step is still going — the status is lost, and so is the step's real
    duration. A test caught this: `exit 7` came back `Success`. The wrapper now
    records the status beside the pgid, and `wait` polls the process group's liveness
    rather than trusting an early return, which makes the executor correct whichever
    way `setsid` behaves. `docker_wait_follows_the_step_not_the_client` pins both the
    status and the duration.

    The same reasoning covers a second way the client lies. After `SIGTERM` the
    wrapper dies at once — it traps nothing — so the client returns while a step that
    *does* trap TERM is still running. Believing it there reports a graceful exit,
    skips the escalation to `SIGKILL`, and leaves the step running. So `wait` waits
    for a recorded status or a dead process group, whichever comes first; `SIGKILL`
    guarantees the second, which is what bounds the loop.
    `docker_cancel_without_a_recorded_status_still_reports_cancelled` pins the path
    where the wrapper is killed before it can record anything.

22. **The ladder lives in the step kind; the driver owns the outer deadline and the
    timeout-versus-cancel decision.** A step reports `Cancelled` however it was
    stopped, because it cannot know why. Only the driver sees which terminal arrived
    first, so it rewrites `Cancelled` to `TimedOut` when its timer got there first.
    This keeps `Control` closed at `Cancel` rather than growing a variant per reason.

23. **Retry jitter is a hash of `(firing, attempt)`, not an RNG.** Jitter's job is
    decorrelating *different* retries so they do not stampede, which a per-firing hash
    does. It avoids a dependency and leaves the driver reproducible, so retry timing
    is testable.

24. **`ResolvedFiring` never actually reaches the event log here.** §6.1 motivates
    the secret-reference amendment with "`ResolvedFiring` is serialized into
    `StartStep` records", but in this implementation commands are not events, so no
    command is ever logged. The amendment is implemented anyway — it is the right
    invariant for any host that persists commands — and the secret test greps the
    whole serialized `EngineState`, not just the log, which is the stronger check.

25. **`split(string, separator)` joined the expression language**, and the language
    now has a gate. The outputs-file protocol yields strings, so a step that produces
    a list of regions produces one string, and `for_each` needs an array — acceptance
    test 1 cannot be written without it.

    Three functions had by then accreted under pressure from individual tests, which
    is how ad-hoc scripting languages are born. `ir::expr::BUILTINS` is now a table
    that **gates dispatch** rather than describing it: a call is looked up there
    before any match arm is reached, and arity is checked once from the entry. A
    function missing from the table is unknown however many arms exist, and an entry
    with no arm fails its own conformance test. The bar for adding one — pure, total,
    tested including error cases, and justified by something that cannot be written
    without it — is documented on the table itself.

26. **Docker images must provide `setsid`.** busybox and util-linux both do, so
    alpine, debian and ubuntu are all fine. A spawn into an image without it fails
    with a spawn error rather than silently losing the process group.

27. **Log draining after a process ends is bounded at 5 seconds.** §3.4 says capture
    continues "until both streams close", which a grandchild holding the pipe open can
    delay indefinitely. The driver's hard deadline would eventually fire, but bounding
    the drain keeps the failure local and legible.

28. **`ScopeSpec`, `EnvHandle` and `Teardown` are defined here.** §1 names the first
    two without giving their shape.

29. **`RunHandle` is how a cancel gets in.** The handoff has `CancelRequested`
    arriving as an event without saying who sends it; `Driver::handle()` returns a
    handle that can inject one into a run in flight.

## Testing notes

Three things this package taught, kept because they generalise:

- **Assert the duration, not just the result.** A `docker exec` that returns early
  looks exactly like a step that finished fast. Pinning elapsed time alongside the
  exit status in `docker_wait_follows_the_step_not_the_client` is what makes that
  regression loud instead of silent.
- **A cancellation test that only checks the process is gone will pass on teardown
  alone.** Releasing the scope kills everything either way, so the test has to pin
  *how* the step ended — which signal, which escalation — not merely that it did.
- **Docker tests skip without a daemon, and `PETRI_REQUIRE_DOCKER` turns that skip
  into a failure.** CI sets it on the Linux job. A silently skipped acceptance
  battery is indistinguishable from a passing one, and that job exists precisely to
  say the battery ran.

## Not built

Out of scope per executor §0, and not built: GHA action shims and the JS action host,
artifact and cache stores, remote or distributed executors, Windows, service
containers, and cpu/memory limits. The `LogSink` writes to the run directory and
optionally stdout; richer sinks come later.

Deliberately out of scope, per the core handoff: outcome-driven splice (§3, deferred —
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
