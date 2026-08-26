# Engine Specification (v2, consolidated)

**Status:** Single source of truth as of ce8de21. Supersedes `ir-design.md`,
`core-gaps-handoff.md`, and `executor-handoff.md` (archive them; do not update
them further). Incorporates every accepted departure from builder READMEs 1–29
and the ce8e follow-ups. Where this document and the code disagree, that is a
bug in one of them — flag it, don't silently pick one.

---

## 1. Model

The engine executes a directed graph (cycles permitted) with token-flow
semantics. A node **fires** when its join policy is satisfied by incoming tokens
of one generation; on completion, routing emits tokens on outgoing edges.
Multiple tokens in flight = concurrent nodes. The run is **complete at
quiescence**: no live firings and no pending token can satisfy any join. Run
status folds from node outcomes under the graph's `Completion` policy:
`AnyFailure` (default — any failed record fails the run, the CI rule) or
`TerminalNode(id)` (success iff that node has a success-like final record at
quiescence; failures elsewhere are control flow, and a missing record is a
failure — the successful nonterminal dead end fails, a pinned departure from
fabro_core). Root cancellation and engine `RunError`s outrank both. The
`run.failed` static keeps its own meaning — "any failure so far" (errors or
failed history), under every policy — deliberately not the folded status,
which under `TerminalNode` would read `Failed` until the exit record exists.

All coordination lives in a pure, sans-IO core: `apply(state, event) ->
(state, commands)` — deterministic, no clocks, no RNG, no filesystem. All side
effects live behind traits (`Executor`, `StepKind`, `LogSink`,
`SecretProvider`, stores).

## 2. Routing: AND-of-XOR normal form

A node's routing is a list of **select groups**. Each group independently emits
**at most one** token: arms in order, first passing guard wins. N groups may
emit N tokens concurrently.

| Pattern | Representation |
|---|---|
| Default: pick one successor | 1 group, N guarded arms (+ fallthrough) |
| Unconditional next | 1 group, 1 arm, `Guard::Always` |
| Fan-out (parallel) | N groups of 1 arm each |
| Conditional fan-out (OR-split) | N groups; a group may emit nothing (`Fallthrough::NoEmit`) |
| Loop back-edge | arm whose edge has `back: true` (increments generation) |

Fan-out requires writing multiple groups; it can never occur implicitly.

## 3. Core types (normative shapes; code is authoritative for detail)

```rust
// Identifiers: NodeId, EdgeId, ScopeId, ExprId, CancelScopeId (u32 newtypes);
// FiringId (u64, unique per run); Generation(u32); Attempt(u32, 1-based).
// Firing key: (NodeId, Generation, Attempt).

pub enum Guard { Always, Expr(ExprId) }

pub struct Edge {
    pub id: EdgeId, pub to: NodeId, pub guard: Guard,
    pub map: Option<ExprId>,      // token payload; default = source outcome.output
    pub back: bool,               // traversal increments Generation
}

pub struct SelectGroup { pub arms: Vec<Edge>, pub fallthrough: Fallthrough }
pub enum Fallthrough { NoEmit, Error }
pub struct Routing { pub groups: Vec<SelectGroup> }

pub enum JoinPolicy { All, Any, Quorum { n: u32 } }   // matched per (node, generation)

pub struct Node {
    pub id: NodeId, pub name: SmolStr, pub scope: ScopeId,
    pub step: StepRef,                     // registry key + config (HIR: may hold ExprIds)
    pub join: JoinPolicy,
    pub precondition: Option<ExprId>,      // false => Skipped without executing; routing still runs
    pub routing: Routing,
    pub budget: Budget,                    // max_firings (counts generations), timeout (per attempt)
    pub retry: RetryPolicy,
    pub run_on_cancel: bool,               // §5: may fire inside a cancelled scope
    pub meta: Value,                       // opaque, host-facing; the core never reads it
    pub expand: Option<Expansion>,         // HIR only
}

pub struct RetryPolicy {
    pub max_attempts: NonZeroU32,          // 1 = no retries (default)
    pub backoff: Backoff,                  // initial, factor, max; jitter applied by driver
    pub retry_on: RetryOn,                 // Vec<StatusKind> + failure classes (§3.1)
    pub on_exhaustion: Exhaustion,         // Fail | AcceptPartial
}

pub enum Expansion {
    ForEach { items: ExprId, target: ExpandTarget,
              max_parallel: Option<u32>, fail_fast: bool },
}
pub enum ExpandTarget { Node, Subgraph { entry: NodeId, exit: NodeId } }

pub struct Scope {
    pub id: ScopeId,
    pub env: BTreeMap<SmolStr, ExprOrValue>,
    pub runtime: RuntimeSpec,      // HostProcess | Docker { image, .. };
                                   // + requirements: Vec<SmolStr> (opaque labels, D3)
    pub workspace: WorkspacePolicy,
}

pub struct Token {
    pub edge: EdgeId,
    pub generation: Generation,    // renamed from `gen` (edition-2024 keyword)
    pub payload: Value, pub from: FiringId,
}

pub enum Completion {               // Graph.completion: how run status folds (§1)
    AnyFailure,                     // default
    TerminalNode(NodeId),
}
```

### 3.1 Status — closed and permanent

```rust
pub enum Status {
    Success,
    PartialSuccess { underlying: Option<FailureInfo> },  // success-like; carries the real failure
    Failure(FailureInfo),                                // FailureInfo.class: SmolStr (§13)
    Skipped, Cancelled, TimedOut,
}
```

Rules (violations are review-blockers):
1. **Closed enum** — six variants, permanent. Frontend concepts map onto them.
2. **One classification point** — `Status::is_success_like()` (`Success |
   PartialSuccess`) is the only definition of success-likeness; joins, cancel
   scopes, default guards, retry defaults all call it. Never open-code the match.
   (`is_success()` and `FailureInfo.retryable` were deleted for violating this;
   do not reintroduce.)
3. **Log truth** — converting a failure to `PartialSuccess` must preserve the
   real failure in `underlying`. All conversion paths: process `soft_fail`
   config; `Exhaustion::AcceptPartial` (fires whenever a retryable status hits
   exhaustion, including `max_attempts: 1`); direct StepKind return.
4. `StatusKind` is the payload-free discriminant for `retry_on` matching only —
   derived via one `From<&Status>` impl; not a second classification point
   (an explicit `PartialSuccess` entry in `retry_on` cannot defeat rule 2).
5. `Status::Cancelled` carries no payload. Cancellation escalation detail lives
   in `output.cancel_escalation: "sigterm" | "sigkill" | "cancel_forced" |
   "cancelled_before_resume" | "killed_before_resume"` (the last two are
   resume's, §10).

## 4. Firing, retries, run context

**Firing rule.** Per (node, generation): satisfy join → check budget → evaluate
`precondition` (false ⇒ synthesize `Skipped`; routing still runs) → emit
`StartStep`. Token generation on emit = source generation, +1 per back edge.
`Cancelled` joins the statuses that flow through routing (§5);
`is_success_like` is untouched — it remains `Success | PartialSuccess`.

**Retries.** Each firing starts at `Attempt(1)`; counters reset per firing (a
later generation retries fresh). On a matching non-final failure the core emits
`ScheduleRetry` (deterministic base delay; driver adds jitter and sleeps; feeds
back `RetryElapsed`). **Retries are invisible everywhere except the event log:**
routing, run-context recording, cancel-scope propagation, and `kv` merges key
off the final attempt only. Non-final attempts' full outcomes (including their
`context_updates`) live in their finish records for tooling. `AcceptPartial`'s
converted outcome *is* final and merges normally. Success-like statuses are
never retried. `Budget.max_firings` counts firings, not attempts;
`Budget.timeout` is per attempt.

**Run context.** Core-maintained, derived state (never checkpointed
separately):

- `nodes.<instance>.{status, output, generation, attempts}` — written only by
  the core on final-attempt `StepFinished`; clones record under instance names
  (`build#2`). The separator is `#`, not `[n]`: instance names become expression
  keys, and `nodes.build[2].status` would collide visually with the `[0]` /
  `['key']` index syntax a frontend grammar has to parse, while `build#2` is
  unambiguously one key.
- `kv.*` — written only via `Outcome.context_updates`, merged in `apply()` in
  event order, last-write-wins.

Expressions evaluate against `EvalEnv { token, run: &RunContext, statics }` —
guards, `map`, preconditions, `items`. This **replaced** the interim
firing-context mechanism; edges no longer thread status. `success()` on an
entry node evaluates true off its seed; on clones it resolves via instance
names.

## 5. Splices, loops, cancel scopes

**Splice** (`Expansion::ForEach`, evaluated inline in `apply`): clone target per
item with `item`/`index` bound; each splice creates a fresh `CancelScope`
(`fail_fast` cancels siblings via it); `max_parallel` is scheduler admission
control. **Supersession rule:** the splice replaces the template region's edges
entirely — a collector's `All` join counts spliced edges only, never a template
edge no token can cross (that was a real deadlock; the regression test is named
for it). Collector ordering uses `index` in token payloads. Spliced tokens
inherit the splicing firing's generation. `Command::ExpandNode` exists as a seam
for external item resolution but is never emitted today.

**Entry nodes** are seeded via synthetic seed edges allocated at runtime from
`max declared id + 1` (collision impossible by construction; validation never
sees them; `EdgeId::SEED` is rejected in routing groups). Join counting is
uniform: `All` over one seed edge = one seed token.

**Sequential for_each** is a frontend desugar, not IR: entry emits
`{items, idx: 0, acc: []}`; final select group has a back arm guarded
`idx + 1 < len(items)` with an accumulating `map`, and an exit arm carrying the
accumulator. Generations distinguish iterations; budgets cap runaway loops.

**Cancel scopes** — dynamic sets of firings cancellable as a unit: the run
root, each splice, job-level cancel-on-failure. Stopping has two tiers, Cancel
and Kill — the workflow-level analogue of `SIGTERM` and `SIGKILL`.

**Cancel** (`Event::CancelRequested { scope }`) asks the scope to stop:

1. Live firings get `DeliverControl { Cancel }`. A cancelled firing's final
   outcome **routes like any other outcome**. Retry is still refused: the point
   of cancelling is to stop the work.
2. Pending tokens are **not** dropped. A node in a cancelled scope whose join
   is satisfied completes `Cancelled` without executing — no `StartStep` —
   unless it opted in via `Node.run_on_cancel`:
   - `run_on_cancel` set → evaluate the precondition. Absent or true → the node
     **fires for real**. False → complete `Cancelled`. An evaluation error
     keeps the ordinary behavior — `RunError::Eval` plus a routed `Failure`
     outcome — because cancellation must not convert a broken expression into a
     clean cancellation.
   - `run_on_cancel` unset → complete `Cancelled` without evaluating anything.
     Un-marked work can never restart, whatever its gates say.
   - An expansion node in a cancelled scope never expands; it completes
     `Cancelled`. `run_on_cancel` on an expansion node is a validation warning
     (ignored in v1).
   - The same admission applies to work **fed by** cancelled work: a node any
     of whose join tokens was emitted by a firing that recorded `Cancelled`
     completes `Cancelled` unless it is marked. This is what keeps a
     `fail_fast` splice's un-marked collector — outside the cancelled scope —
     from starting, while a marked one fires and gathers partial results.
3. A firing **awaiting a retry backoff** has no work in flight and no driver
   task to deliver to, so the core settles it at once instead of waiting out
   the backoff: it records a `Cancelled` outcome and routes it (under Kill:
   records without routing). Settling leaves a tombstone; the one matching late
   `RetryElapsed` consumes it silently — the driver's sleeper cannot be
   recalled, and replay must stay clean. Every other invalid `RetryElapsed` —
   unknown firing, not awaiting, duplicate after the tombstone is consumed —
   still raises `UnknownFiring` / `UnexpectedRetry`; the no-op is
   cancellation-specific, never a blanket swallow of malformed input.

**Kill** (`Event::KillRequested { scope }`) stops the scope: the forced tier,
scope-addressed like `CancelRequested` (a kill of `ROOT` kills the run).
Killing marks the scope closure killed (killed implies cancelled), drops its
pending and deferred tokens, swallows tokens aimed inside it, records live
firings' outcomes **without routing**, and admits nothing — `run_on_cancel`
included. Delivery is `Control::Kill`, sent to **every** live firing in the
closure, already-cancelling ones included; a step kind receiving it goes
straight to `SIGKILL`, no ladder. No new `Status` or `RunStatus` variant: how a
firing was stopped is a mode, not an outcome — a killed firing records
`Cancelled`, and the logged `KillRequested` event carries the mode.

Expressions see cancellation through two statics: `run.cancelled` (root-only,
built from the folded run status) and `scope_cancelled` (true when the firing's
node lies in a cancelled cancel-scope; a root cancel marks every scope, so it
subsumes `run.cancelled` for gating). The upstream status fold has a
`cancelled` arm — failure > cancelled > skipped > success — so the core
`success()` guard is false and `cancelled()` true over a cancelled upstream.

**Terminal scope release:** when the run finishes, the core emits
`ReleaseScope` for every still-held scope and removes them from `held_scopes`
in the same transition, before `FinishRun` — a finished serialized state claims
no resources. Nothing can need an environment after `FinishRun`. (This also
covers the parked-token leak that exists independently of cancellation: a token
parked at an unsatisfiable join no longer holds its environment past the end of
the run.)

After a cancel the core routes and fires whatever `run_on_cancel` admits;
**bounding** cleanup is the driver's job (§10): a cleanup that outlives the
grace is ended by feeding back `KillRequested`.

`Control::Deliver` is not a stop signal: it never starts the cancellation
ladder or the kill tier, and delivering one touches no cancel-scope state.

A hierarchical `SubgraphStep` (nested scheduler) remains rejected: loops and
matrices always flatten into the one graph.

## 6. Engine interface, event log, ResolvedFiring

Events: `RunStarted`, `TokenEmitted`, `StepStarted{firing, attempt}`,
`StepProgress`, `StepFinished{firing, attempt, outcome}`, `RetryElapsed`,
`NodeExpanded`, `CancelRequested{scope}`, `KillRequested{scope}`,
`ControlRequested{firing, ctl}`. Commands: `StartStep(ResolvedFiring)`,
`DeliverControl`, `ScheduleRetry`, `ExpandNode` (reserved), `AcquireScope`,
`ReleaseScope`, `FinishRun`.

**`ControlRequested`** is the host delivering a value into a live firing — a
human gate's answer, a supervisor's steering — through the engine, so question
and answer are both in the log and replay/resume reproduce a pending
interaction. Only `Control::Deliver` to a live, not-cancelling,
not-awaiting-retry firing emits `DeliverControl`; no state change, no routing
effect. Everything else is a **logged no-op**, never a `RunError`: a dead or
unknown firing (a late answer must not fail the run), and `Cancel`/`Kill`,
whose own scope-routed events carry closure bookkeeping (`cancelling`, kill
tiers, `run_on_cancel` admission) a raw per-firing path would bypass. The
command-or-no-command result of `apply` is the disposition the driver reports.

**Log v3 + EventSource.** Append-then-apply; every record carries
`EventSource::{External, Core}` (closed enum). Replay feeds back **External
only**; the core must regenerate its own events byte-identically — that
regeneration is the determinism assertion, not redundancy. Arrival order at the
driver's single mpsc is canonical; all wall-clock nondeterminism (completion
order, retry timing, races) is captured in External events.

**Observers.** The driver's `EventObserver` is the host-facing record stream:
every appended record, External and Core, in seq order, exactly once per
driver lifetime — at-least-once across a resume, deduped by `(log identity,
seq)` (`seq` is per-log; the identity is host-named, stable across resume,
fresh per fork) — with the post-apply `EngineState` alongside, so a consumer
resolves a firing to its node, name and `meta` in place
(`EngineState::firing_node`, which searches live firings and history both). A
callback, deliberately not a broadcast channel: broadcast drops on lag, and a
store ingest must never lose a record.

**Persistence surface.** The core's whole persistence surface is `EventLog`'s
serde plus `EventLog::try_from_records(version, records)` (version checked
under the standing no-migrator policy; seqs contiguous from 0). How records
are framed and stored is the host's business; the stock run-dir file
convention (`events.jsonl` + `graph.json`) is the standalone petri host's own
and is documented with it, not here.

**Resume.** `engine::resume(graph, &log)` rebuilds a crashed run by replay and
reconciles what is still owed. The loaded log must be a **byte-prefix** of the
regenerated one — not equal: a crash can land between an External append and
the flush of the Core records it derived, so the regenerated log may be
longer; anything else is real divergence and refuses with `ReplayMismatch`.
Because `apply` is deterministic, every `StartStep` and `ScheduleRetry` is
regenerated byte-identically — nothing about in-flight work needs separate
persistence. `ResumePoint` carries the rebuilt state, the pending commands in
dispatch order (`AcquireScope` per held scope; per live firing the last
`StartStep` — unless awaiting a retry, whose `ScheduleRetry` is re-armed
instead, the sets disjoint because `awaiting_retry()` ⊂ `live_firings()`), and
the re-dispatched firings, which are the host's view: **resume is invisible in
the log** — no marker event, no `LOG_VERSION` bump — so a resumed execution
reaches the same state from the same records and `verify_replay` keeps meaning
something; a host mints new execution identities from `ResumePoint`/
`ResumeInfo`, never from a log event. `EventLog::prefix(len)` is the
rewind/fork helper: resuming a truncated log is rewind, doing it under a fresh
log identity is fork. Observer delivery across a resume is at-least-once,
deduped by `(log identity, seq)` as above.

**ResolvedFiring** is the fully-bound payload of `StartStep` — the "second IR",
scoped to a firing (the graph itself is the **live graph**, mutated by
splices; "HIR-ness" is a per-node property). Constructor invariant, enforced in
`ResolvedFiring::new` and on deserialization (`serde(try_from)`): **no
unresolved expression placeholders; `{"$secret": "NAME"}` is the one permitted
non-literal form** (§11). A placeholder surviving resolution fails the node
with `UnresolvedConfig` naming the path; `placeholder_path` is shared between
plan validation and the boundary check. Commands are not persisted today, so
the invariant is defense-in-depth locally — but it becomes load-bearing when v2
distribution serializes commands to remote agents. The secret test greps the
whole serialized `EngineState`, which is strictly stronger than grepping the
log; keep that form.

## 7. Expression language

Small, **total**: missing fields evaluate to `null`; a guard always yields a
boolean; no user lambdas. Function calls dispatch **through**
`ir::expr::BUILTINS` — a closed, enumerable table that gates dispatch (arity
checked from the entry; a function absent from the table cannot be called even
if a match arm exists; every entry has a conformance test). 19 entries as of
ce8de21; the code is the authoritative list. **Growth bar, documented on the
table:** pure, total, tested, and justified by an acceptance test that cannot be
written without it (how `split`, `sort_by_key`, `pluck` earned entry — and
`matches`, full-`regex` unanchored search, which a frontend condition grammar
with a regex operator cannot lower without). This
table is the target surface for the GHA `${{ }}` grammar (package 03). A
strict/unknown-field-lint mode is a v2 seam.

## 8. Validation

**Load-time invariants:**
1. Every cycle contains ≥ 1 back edge.
2. `Guard::Always` only as a group's final arm.
3. Groups non-empty; empty `Routing.groups` = terminal node.
4. `max_firings ≥ 1`; any node reachable via a back edge has a finite budget.
5. `EdgeId`s unique; `EdgeId::SEED` rejected in routing groups.
6. Expression references resolve; HIR-only fields absent from executable plans.
7. `ExpandTarget::Subgraph{entry, exit}`: exit postdominates entry; no edges
   cross the boundary except into entry / out of exit.
8. **Any node with an incoming back edge has `JoinPolicy::Any`** — forward
   edges carry only generation 0, back edges only ≥ 1; `All`/`Quorum` over both
   is unsatisfiable for every generation. Deliberately strict: `Quorum{1}` is
   rejected too (one canonical spelling; frontends normalize `Quorum{1}` →
   `Any` in lowering rather than relaxing this). Corollary for users: a node
   cannot be both a multi-branch `All` join and a loop head — put a join node
   in front of the loop head. Entry-node seeding checks consider forward edges
   only (an entry node may also be a loop head).
9. `Completion::TerminalNode(id)`: the node must exist. Nothing more — the node
   is *expected* to be terminal, but the semantics only need a final record, so
   terminal shape and reachability rules belong to frontends.

**Lint (warning, not error):** possible scope re-entry after release — a node
outside a scope both reachable from it and reaching back into it. Suppressed
when every re-entry node's join is `All` with ≥ 1 incoming forward edge from
inside the scope (sound: that arm either pins the scope via a pending token or
renders the join unsatisfiable). The suppression's `!back` filter is
unreachable given invariant 8 and is kept as documented defense. Remaining
positives are a labelled over-approximation; the warning names the re-entry
node and the fix.

**Firing-time errors are node failures, routable, never run aborts:**
`UnresolvedConfig`, `secret_misplaced`, `bad_output_file`, `env_acquire`.

## 9. Scopes and environments

A scope is a resource scope (workspace, container, env, secrets), not a
sequence. **Held, not refcounted:** acquired before the first firing needs it,
released once no firing, pending token, or deferred join needs it (per-firing
refcounting tears a job down between consecutive steps — rejected). **Release
is irreversible;** re-entry acquires a fresh environment (the §8 lint is the
static counterpart). Acquire failure fails every pending firing in the scope
instance with class `env_acquire` via ordinary `StepFinished` events.

**`acquire` fences prior work.** A driver crash kills no running step (release
owns cleanup; remote sandboxes outlive workers by design), so the `Executor`
contract carries one more rule: when `acquire` returns, no process from a
previous acquisition of that scope can still mutate the workspace or be
observed as this environment's status. The host executor implements it with
generation-scoped `groups/<generation>/` records and a publication handshake
(the sentinel durably records its pgid, then checks a `fenced` marker, only
then spawns the workload; the fencer writes markers before reading records) —
and while anything of a discovered group lives, the group's own in-group
watcher is the killer; the fencer only waits for it to drain. **Nothing ever
signals a bare recorded pgid** — it can be recycled to an innocent — so a group
that never drains fails the acquire with the typed `FenceLeaked` error, and the
scope's firings fail routably; cleanup belongs to the operator or host policy
(an identity-bound kill via Linux `pidfd` is an optional platform upgrade,
never a requirement). Docker's fence is remove-by-deterministic-name, which
requires the resuming host to construct its executor with the run's original
id. The fence is idempotent and covers the workspace only: side effects
outside it may have happened in the crashed attempt and happen again — resume
is **at-least-once for external side effects**, exactly-once only for the log
and the workspace fence.

Host executor: workspace per scope instance under the run dir; retention
default keep-on-failure (`always|on_failure|never`). Docker executor: container
per scope instance — pull `if-not-present`, `--init`, workspace bind-mounted
from the host run dir, long-lived init command; steps via `docker exec`;
release = TERM, grace, `rm -f`. Image contract: must provide `setsid`
(busybox/util-linux both do). Parallel steps sharing a workspace: declared file
conflicts or isolated overlays remain future work; v1 native format should not
encourage intra-scope parallel writes to the same paths.

## 10. Driver, process StepKind, cancellation

**Layering:** Driver owns IO scheduling and translates (no policy); Executor
owns environments (`acquire`/`release` + the `ExecEnv` spawn capability handed
to steps); StepKind owns step semantics and never mentions host-vs-Docker.

**Capabilities.** `StepCtx` carries a typed, host-registered capability map
(`Capabilities`, in the `steps` crate): components define concrete handle
types and register values — on the `Runtime` builder or per run via
`Driver::with_capabilities` — and a step asks by type; **the core never names
a capability**. The key is the concrete type (`Arc<dyn Any>` downcasts only to
sized types), so a `dyn`-trait service rides behind a concrete newtype.
Duplicate registration panics, like step registration. `require_capability`
fails routably with class `capability_unavailable` (§13), mirroring
`secret_unavailable`. Capabilities live entirely on the effects side: nothing
touches the engine, the log, or determinism.

**Driver rules:** single command consumer, single External-event producer;
per-attempt timeout timers (expiry → cancellation ladder → `TimedOut`); retry
jitter + sleep → `RetryElapsed`; hard deadline after `Control::Cancel` of
`grace + 5s`, then task abort and synthesized `Cancelled` with
`cancel_escalation: "cancel_forced"`. `release` never fails the run. Observers
are notified after every apply, and the driver awaits every observer's
`finish` before building the report; observer failures surface in
`RunReport.observer_errors` and never change the run status (a host with
fatal-sink semantics watches its own observer and cancels via `RunHandle`).
The stock petri host persists a post-mask run dir through an observer battery;
details live with that host, not here.

**Delivery.** A `DeliverControl{Deliver}` only forwards to the firing's control
channel — no deadline, no reason: a delivered value never starts the
cancellation ladder or the kill tier. Delivery is **reliable, not `try_send`**:
every control send rides a per-firing serialized forwarder that awaits channel
capacity, so a full channel never blocks the driver loop and sends land in
order (channel capacity is `CONTROL_CHANNEL_CAPACITY = 32`, a named
implementation constant, not a compatibility rule). `RunHandle::deliver`
returns a disposition the forwarder completes: `Delivered` only after the send
lands; `NotLive` when `apply` emitted no command or the firing ended first.
Best-effort steering drop semantics live in the host hub, on top of this
reliable primitive.

**Resume rules.** `Driver::resume(graph, log, …)` is the primary API — the
host hands in the graph and log however it stored them — and returns
`ResumeInfo` beside the driver so execution identities are installed before
`run()`. On the resume path `run()` skips `RunStarted`, notifies every
observer of the regenerated suffix **before** dispatching any pending command,
then enters the normal loop. Dispatch differences: a firing whose `started`
flag is set gets no second `StepStarted` ack (it is already in the log); a
firing marked `cancelling` — either tier — is **never re-spawned**: the driver
finishes it directly with a `Cancelled` outcome carrying the per-tier
`cancel_escalation` value (`cancelled_before_resume` / `killed_before_resume`;
data values, not new vocabulary), the tier read from the replayed state, the
polite outcome routing exactly as a live cancel's would while `run_on_cancel`
cleanup firings re-dispatch normally. Timers restart in full — the log has no
clock, so a pending retry waits its whole base delay again and per-attempt
timeouts start fresh. **No automatic re-delivery:** a logged
`ControlRequested{Deliver}` is not re-forwarded — the resumed step waits again
and the host re-sends what its own store says is outstanding, re-registering
dynamic secrets (`answer:<id>`) first or the delivery fails the step with
`secret_unavailable`. External side effects are at-least-once across a resume
(§9).

**Two-tier stop wiring.** The driver never decides to stop the run by itself;
it feeds events and the core decides. The first root `cancel` feeds
`CancelRequested { ROOT }` and arms the cleanup-grace timer
(`RunConfig.cleanup_grace`, default 2 minutes, per-run override). Timer expiry,
or a second root `cancel` (a CLI maps a second Ctrl-C to it), feeds
`KillRequested { ROOT }`. Both are ordinary External events, so the hard stop
is in the log and replay reproduces it. After a Kill the core emits no further
`StartStep`s; the driver's `DeliverControl { Kill }` reaches every live task —
including ones already politely cancelling — and arms a zero-slack hard
deadline as the backstop for a step kind that ignores it.

**Process StepKind:** `bash -eo pipefail -c <run>` (or `sh`); env may contain
secret refs; no step-level timeout (node budget governs). Outcome mapping: 0 →
`Success`; N≠0 with `soft_fail` match → `PartialSuccess{underlying:
exit_status:N}`; N≠0 → `Failure{exit_status:N}`; foreign signal →
`Failure{signal:S}`; our ladder → `Cancelled`/`TimedOut`. Outputs-file
protocol: `CI_OUTPUT` env points to a per-firing file; `key=value` + GHA
heredoc form parsed into `Outcome.output` (plus `output.exit_status`); parse
failure = `bad_output_file`. Log capture: **one merged, arrival-ordered,
stream-tagged `lines()` stream** (two receivers cannot recover an order never
recorded); 64 KiB line cap with truncation marker; capture drains through
cancellation until both streams close.

**Cancellation (one ladder for cancel and timeout):** TERM to the **process
group** → grace (default 10s, per-scope) → KILL to the group. `Control::Kill`
skips the ladder: straight to KILL, no grace. Host: each spawn starts the group
with a **sentinel** supervisor — the group leader, which runs the workload as a
member of the same group, reports its exit status out of band, closes its
inherited copies of the stdout/stderr pipes after the spawn (so the pipes reach
EOF when the workload exits), ignores `SIGTERM` so the polite ladder passes
through it, and stays alive until scope release. While the sentinel lives the
group is never empty, so the kernel cannot recycle the pgid; the executor owns
the sentinel's unreaped handle, so even a killed sentinel pins the id as a
zombie. Release sends one `killpg(SIGKILL)` **while the id is still pinned**,
reaps the sentinel, then performs only **non-signalling** bounded observation
of group death (procfs on Linux, libproc on macOS) — never a signal after the
reap frees the id, and never an `ESRCH` probe, which a zombie leader defeats. A
group that outlives the deadline is a report entry, and no zombie outlives
release. All other signalling stays `killpg` only. Docker: `docker kill` reaches PID 1 only —
step-level signalling is `docker exec <c> kill -TERM -<PGID>` (**no `--`
separator**: busybox `kill` rejects it and a rejected signal is a silent one;
this corrects the original handoff text). The in-container wrapper records the
step's exit status beside its pgid via **atomic write (temp + rename)**;
`wait` follows **a recorded status or group death, whichever first** — never
the exec client's return (setsid may fork; a trapping step outlives the
client). Liveness polling interval is the documented `LIVENESS_POLL` constant.
Races: exactly one terminal `StepFinished` per firing; first terminal wins
(natural exit vs ladder; timeout vs cancel by driver arrival order — the log
then makes it canonical, so replay agrees whichever way it landed).
Post-conditions: streams drained, outputs file still parsed if present,
release still runs, nothing leaked (no workspace, container, or process
group). Known limitation: host-executor double-fork daemons escape the group;
Docker's PID namespace is the mitigation.

## 11. Secrets

Secret **values** never enter the event log or serialized state. Config carries
`{"$secret": "NAME"}` through `ResolvedFiring`; resolution happens at
spawn-time via `SecretProvider` directly into the child env. `$secret` is valid
only in env-shaped positions (`secret_misplaced` otherwise); secrets are absent
from `EvalEnv` — guards cannot read them by construction. Masking (exact-match
→ `***`, min length 6, multiline masked per line) applies before append to:
`StepEvent::Log` lines, `StepEvent::Artifact` names and uris, and all string
values in `StepEvent::Custom`, `Outcome.output` and `context_updates`. Encoded
variants (base64/urlencoded) are a documented v2 gap.

`Graph.params` are recorded data: a raw secret value never belongs in them —
the graph is persisted as it ran (replay needs it byte-exact, so a masked copy
would be a different graph), and this contract is what makes that safe. The
standalone petri host adds a checkable backstop: if the masker already
recognizes a value in the serialized graph, it refuses to start the run rather
than persist. Exact-value masking of registered values is the driver's job;
pattern-based redaction beyond registered values is the **emitting step's**
job before it sends — a product's redaction policy does not belong in the
core.

A sensitive `Deliver` payload crosses the same way: `{"$secret": "answer:<id>"}`
in the event (the log keeps the reference), resolved by the driver at
command-dispatch time into the step. `SecretProvider::register(name, value)`
lets the host add a dynamic value after run start (default: a typed unsupported
error); duplicate names are rejected so an answer id can never shadow a
configured secret, and registration feeds the masker, so anything resolvable is
maskable by construction — the lifetime is the provider instance, i.e. the run.
Dynamic values are not in the log by design, so after a crash the host must
re-provide them on resume; an unresolvable reference at dispatch fails the step
with `secret_unavailable`.

## 12. Frontend lowering contracts

**GHA** (exercises the degenerate subset — no back edges, no `Any`/`Quorum`,
no multi-arm groups): job → Scope + chained step nodes; `needs` → `All` join;
k dependents → k single-arm groups; `if:` → precondition; `strategy.matrix`
(+`fail-fast`/`max-parallel`) → `ForEach{Subgraph}`; `timeout-minutes` →
budget; `continue-on-error` → `soft_fail` (context provider derives
`outcome`/`conclusion` from `PartialSuccess.underlying`); `runs-on` →
`RuntimeSpec.requirements` (executor maps known labels, rejects unknown
per-label); composite actions inline. **Rejected loudly, never
parse-and-ignore** (shared `Unsupported{feature, hint}` diagnostic): GHA
`concurrency:`, BuildKite `concurrency_group` (D2), and anything else
unimplemented. Native format: `next:` → one group; `parallel:` → multiple
groups; `for_each` + `parallel: true|false` → `ForEach` vs cycle desugar.

## 13. Failure-class registry (grep anchor; extend here first)

**Step outcome classes** (`FailureInfo.class`, matchable by `retry_on`):

| Class | Raised when |
|---|---|
| `exit_status:N` | the process exited non-zero |
| `signal:S` | the process was killed by a signal that was not ours |
| `retry_requested` | a step asked to be run again (Attractor's `RETRY`) |
| `bad_output_file` | the outputs file did not parse |
| `bad_config` | the step's config did not deserialize |
| `secret_misplaced` | a `$secret` ref outside an env-shaped position |
| `secret_unavailable` | a named secret is not configured for this run |
| `workspace_setup` | the workspace could not be prepared for the step |
| `spawn_failed` | the process could not be started at all |
| `env_acquire` | the scope's environment could not be materialized |
| `no_runner` | no step kind is registered for a node's `StepRef.kind` |
| `capability_unavailable` | a step required a host capability no one registered (§10) |

Not classes, listed here so they are findable: `cancel_forced` is a value of
`output.cancel_escalation` (§3.1 rule 5); `UnresolvedConfig` is an error type that
surfaces as a node failure; `invalid_splice` is reserved for the deferred
outcome-driven splice.

Names say what went wrong, so a `retry_on` entry reads as a policy rather than a
riddle — this is why `spawn` and `workspace` became `spawn_failed` and
`workspace_setup`. `no_runner` is caught at load: the one step registry
implements the lookup `validate_with` takes, so a node naming a kind with no
runner is a validation error, and the driver's firing-time guard is the backstop
for a caller that skipped validation.

## 14. Deferred and v2 seams (build nothing here)

Outcome-driven splice / BuildKite pipeline upload (`Outcome.splice`,
`allow_splice` — additive later). Cross-run concurrency: driver-layer service
(D2); eventual `concurrency_key` is additive. Placement semantics beyond
opaque labels (D3). `Control::Pause` (enum is `#[non_exhaustive]`); `Steer` and
`Approve` shipped as `Control::Deliver` (§6, §10). Strict expression mode. Encoded-secret masking. Content
caching (`StepKind::fingerprint` defaults `None`). JS action host; action
shims are package 04. Windows; service containers; resource limits.

## 15. Testing notes (institutional memory)

- **Trace green tests to their assertion.** Both major Docker bugs hid behind
  tests that could not fail (group-kill "passing" because release removed the
  container; graceful-exit reported because the exec client returned early).
  When a test guards a signal path, assert the *mechanism* (escalation level,
  who ended the process), not just the end state.
- **Assert the duration, not just the result.** Elapsed time was the tell that
  `wait` was following the client instead of the step.
- **Determinism canaries:** replay byte-identity on every E2E; the raced tests
  run repeatedly and assert replay agrees whichever way the race lands.
- Acceptance batteries live with their packages (core §7 tests 1–5; executor
  §7 tests 1–10) and are named for the rules they pin.
