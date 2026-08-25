# Handoff: Core Semantics Patch 01

**Applies to:** `ir-design.md` (prior version, already under implementation)
**Purpose:** Close the gaps found by validating the IR against GHA, BuildKite, RWX,
and Attractor. Two definite changes (§1–§2), one deferral (§3), resolved decision
D1 (§4), pending decisions D2/D3, two mechanical migrations (§5).

**Event log format bumps v1 → v2** (new events, extended firing key). Do this first;
everything below rides on it.

---

## 1. Retries: attempt dimension + RetryPolicy

**Why:** BuildKite `retry`, Attractor `max_retries`+backoff, RWX task retries. A
retry is *not* a loop iteration, so it must not touch `Generation`.

### 1.1 Types

```rust
pub struct Attempt(u32);   // 1-based; firing key becomes (NodeId, Generation, Attempt)

pub struct RetryPolicy {
    pub max_attempts: NonZeroU32,      // 1 = no retries (default)
    pub backoff: Backoff,
    pub retry_on: RetryOn,
    pub on_exhaustion: Exhaustion,     // Fail (default) | AcceptPartial (see §4)
}

pub struct Backoff {
    pub initial: Duration,
    pub factor: f64,                   // exponential; 1.0 = linear
    pub max: Duration,
    pub jitter: bool,                  // applied by the DRIVER, not the core (§1.3)
}

/// Predicate over the failed attempt's outcome. Success-like statuses
/// (per Status::is_success_like, §4) are never retried.
pub struct RetryOn {
    pub statuses: SmallVec<Status>,        // e.g. [Failure, TimedOut]
    pub failure_classes: SmallVec<SmolStr>, // matches FailureInfo.class (§1.4)
}

// Node gains:
pub struct Node {
    // ...existing fields...
    pub retry: RetryPolicy,
}
```

### 1.2 Core semantics

- Each firing of `(node, gen)` starts at `Attempt(1)`. Counters never carry across
  firings — a later generation retries from scratch (matches Attractor's
  reset-on-success for free).
- On an attempt outcome matching `retry_on` with attempts remaining, the core emits
  `Command::ScheduleRetry` and does **not** run routing. Routing, run-context
  recording of the final status, and cancel-scope failure propagation all key off
  the **final attempt only**. Intermediate attempts are still in the event log and
  in `nodes.<id>.attempts` (§2).
- `RunContext.kv` merges on the **final attempt only**, like the node records. Guards
  read `kv` concurrently, so merging a retried attempt's `context_updates` would let
  work that was later discarded steer routing elsewhere in the graph; retries must be
  invisible everywhere except the event log. Per-attempt data is not lost — every
  attempt's finish record carries its full outcome, `context_updates` included.
- `Budget.max_firings` counts firings (generations), **not** attempts.
  `Budget.timeout` applies **per attempt**. (Node-total wall clock: not now.)

### 1.3 Events/commands (determinism rule)

```rust
// Command
ScheduleRetry { firing: FiringId, next_attempt: Attempt, base_delay: Duration },
// Event
RetryElapsed  { firing: FiringId, next_attempt: Attempt },
```

The core computes `base_delay` deterministically (`initial * factor^(n-1)`, capped).
The driver applies jitter and wall-clock sleeping, then feeds `RetryElapsed` back —
same pattern as timeouts. Replay stays deterministic because attempts are
event-driven; the core never sees a clock or RNG.

### 1.4 Failure classes

Add `class: SmolStr` to `FailureInfo` (e.g. `"network"`, `"rate_limit"`,
`"exit_status:2"`, `"retry_requested"`). StepKinds set it; `retry_on` matches it.
Attractor's first-class `RETRY` outcome lowers to
`Status::Failure { class: "retry_requested" }` + `retry_on` matching that class —
do **not** add a `Retry` variant to `Status`.

### 1.5 Frontend mappings (for the compat corpus)

| Source | Lowering |
|---|---|
| BuildKite `retry.automatic: [{exit_status: N, limit: L}]` | `max_attempts: L+1`, `retry_on.failure_classes: ["exit_status:N"]` |
| Attractor `max_retries: R` + backoff presets | `max_attempts: R+1`, `Backoff` fields direct |
| GHA (no native step retry) | default policy (max_attempts 1) |

---

## 2. Run context: run-scoped state readable by expressions

**Why:** Attractor goal gates read outcomes of arbitrary previously-visited nodes at
the exit node; its handlers share a KV store via `context_updates`. GHA's
`success()`/`failure()` job-status semantics also want this instead of being
smuggled through token payloads.

### 2.1 Types

```rust
pub struct RunContext {
    /// Written ONLY by the core, automatically, per completed firing.
    pub nodes: BTreeMap<SmolStr, NodeRecord>,   // key: node instance name
    /// Written ONLY via Outcome.context_updates, merged in apply().
    pub kv: BTreeMap<SmolStr, Value>,
}

pub struct NodeRecord {
    pub status: Status,          // final attempt, raw
    pub output: Value,
    pub gen: Generation,         // latest generation to complete
    pub attempts: u32,
}

// Outcome gains:
pub struct Outcome {
    // ...existing fields...
    pub context_updates: BTreeMap<SmolStr, Value>,   // default empty
}
```

### 2.2 Write rule (this is the load-bearing part)

All writes happen inside `apply()`, in event order: node records on `StepFinished`
(final attempt), `kv` merges last-write-wins in the same order. No other write path
exists. This keeps the core pure and replay byte-identical. `RunContext` is derived
state — reconstructible from the event log; do not checkpoint it separately.

Matrix/splice clones record under their instance name (e.g. `build[2]`), consistent
with how clones are already named.

### 2.3 Expression environment

Evaluator signature changes from `(token, statics)` to:

```rust
pub struct EvalEnv<'a> {
    pub token: &'a Value,
    pub run: &'a RunContext,     // exposed as `nodes.<id>.*` and `kv.*`
    pub statics: &'a StaticCtx,
}
```

Guards, `map`, preconditions, and `Expansion.items` all take `EvalEnv`. This is the
mechanical migration with the widest blast radius — do it as one PR.

**Attractor goal gates then lower with no engine feature:** exit node's select group
= arms guarded on `nodes.<gate>.status` checks, back-edge arms to retry targets,
exit arm last.

---

## 3. Outcome-driven splice — DEFERRED (do not implement)

Cut from this patch by decision; BuildKite `pipeline upload` support moves out with
it (consistent with the BuildKite frontend already being post-v1). Deferring is
safe because:

- The splice *mechanism* ships anyway via `Expansion::ForEach` — `NodeExpanded`,
  cancel scopes, instance namespacing. Outcome-driven splicing later is a new
  entry point into an existing path, not new machinery.
- The fields it would add (`Outcome.splice`, `Node.allow_splice`) are optional and
  additive — a backward-compatible event-log change later, not a format bump.

Nothing to build. Section retained as a stub so section and decision numbering in
review threads stay stable.

---

## 4. D1 resolved: `PartialSuccess` is first-class

Decision: extend `Status` with exactly one variant. No `on_error` policy, no
raw/effective status split.

```rust
pub enum Status {
    Success,
    /// Soft failure or partial completion. Routing-visible, success-like.
    PartialSuccess { underlying: Option<FailureInfo> },
    Failure(FailureInfo),
    Skipped,
    Cancelled,
    TimedOut,
}
```

Three rules contain the known costs of this choice:

1. **Closed enum.** These six variants are the complete, permanent status
   vocabulary; any future frontend concept must map onto them, never extend them.
   Record this in a doc comment on the enum.
2. **One classification point.** `Status::is_success_like()` (`Success |
   PartialSuccess`) is the *only* definition of success-likeness. Joins, cancel
   scopes, default success guards, and retry defaults call it — never open-code
   that match. Guards may still test `partial_success` explicitly (goal gates,
   fan-in ranking).
3. **Log truth preserved.** Any conversion of a failure into `PartialSuccess`
   must carry the real failure in `underlying`. The log never records a clean
   success for something that failed.

Production paths (no node-level policy exists — these are the only three):

- **StepKind config:** process step gains `soft_fail: true | [exit_statuses]`
  (BuildKite-shaped); on match it returns `PartialSuccess` with `underlying`
  populated from the exit status.
- **Retry exhaustion:** `RetryPolicy` gains `on_exhaustion:
  Exhaustion::{Fail, AcceptPartial}` (Attractor `allow_partial`).
- **Direct:** any StepKind may return it (Attractor `PARTIAL_SUCCESS`).

| Source | Lowering |
|---|---|
| GHA `continue-on-error: true` | `soft_fail` on step config; GHA context provider derives `steps.x.outcome = failure`, `conclusion = success` from `PartialSuccess.underlying` |
| BuildKite `soft_fail` (+ exit statuses) | `soft_fail` config, direct |
| Attractor `allow_partial` | `on_exhaustion: AcceptPartial` |
| Attractor handler returns `PARTIAL_SUCCESS` | direct |

**D2 resolved:** cross-run concurrency groups live in the multi-run driver layer,
later — no IR field now. v1 frontends must **reject** workflows using GHA
`concurrency:` / BuildKite `concurrency_group` with a clear unsupported-feature
message; never parse-and-ignore a mutual-exclusion feature. The eventual
`concurrency_key: Option<ExprId>` is additive when it comes.

**D3 resolved:** placement hints are opaque data now, semantics in v2. `RuntimeSpec`
gains `requirements: Vec<SmolStr>` — uninterpreted labels populated by frontends
(GHA `runs-on`, BuildKite agent tags). The v1 local executor maps known labels
(e.g. `ubuntu-latest` → its Docker image) and **rejects** unknown ones with an
unsupported-target message, per-label. No matching rules, queues, or capability
types until a distributed agent system exists to consume them.

All decisions D1–D3 are now resolved; nothing in this document is pending. Decided
out of scope: Attractor fidelity/model-stylesheet (pure StepKind config).

---

## 5. Migration checklist

1. Event log v2: extend firing key with `Attempt`; add `ScheduleRetry`/`RetryElapsed`
   and `context_updates` on finish records. Write a v1→v2
   log migrator only if you have runs worth keeping; otherwise reject v1 logs
   cleanly.
2. `StepStarted`/`StepFinished` events gain `attempt: Attempt`.
3. Evaluator signature → `EvalEnv` everywhere (guards, map, precondition, items).
4. `FailureInfo.class` threaded through all StepKind impls (process step: populate
   `exit_status:N`).
5. Rename per the terminology fix already agreed: "concrete execution plan" →
   **live graph** (post-splice, in-place) + **ResolvedFiring** (the fully-bound,
   no-`ExprId` payload of `Command::StartStep`). Enforce "no unresolved exprs cross
   the executor boundary" in `ResolvedFiring`'s constructor.

## 6. No new machinery required (don't build these)

- Attractor 5-step edge selection → compiles to one ordered `SelectGroup` (guards
  synthesized from `preferred_label` / `suggested_next_ids` in outcome; weight +
  lexical tiebreak resolved statically into arm order).
- Attractor `wait.human` → a StepKind completing on an external answer event
  (ship `AutoApprove` impl for tests).
- Attractor `manager_loop` → StepKind supervising a **separate run** via the engine
  API; never a nested scheduler.
- Attractor `loop_restart` → driver-level run restart, not core.
- GHA job-status (`success()` in step preconditions) → now reads `nodes.*` /
  scope-accumulated records from §2 instead of token-payload threading.

## 7. New regression tests

1. Retry: node fails twice then succeeds (`max_attempts: 3`) — routing fires once,
   on the final outcome; log shows three attempts; replay is byte-identical.
2. Retry × loop: retried node on a back-edge — attempt resets each generation;
   budget counts 1 firing per generation.
3. Run context: Attractor-style goal gate — exit blocked, jump to retry target,
   gate satisfied on second pass, exit. Entirely via guards on `nodes.*`.
4. `context_updates` merge order: two parallel nodes writing the same key —
   last event wins, replay identical.
5. PartialSuccess: soft-failed process step (exit 1, `soft_fail: [1]`) — joins and
   default success guards treat it as success; a `partial_success` guard can still
   route it distinctly; log retains `underlying` exit status; retry never triggers.
