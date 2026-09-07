# Engine IR

A Rust implementation of [`engine-spec.md`](engine-spec.md): a token-flow graph with
explicit routing, plus the pure state machine that executes it.

```
crates/core/ir         the vocabulary: graph, ids, expressions, values, validation
crates/core/engine     the sans-IO state machine: apply(state, event) -> (state, commands)
crates/core/executor   the environment interface; executor-sandbox implements it
crates/core/executor-sandbox  scopes and leases over Host, Docker, and Daytona plugins
crates/core/steps      step kinds and the one registry; frontends depend on names, not on this
crates/core/driver     the IO loop between the pure core and real processes
crates/core/execution  run, invocation, and execution coordination; durable local store
crates/core/frontend   what every format shares; frontend-native is core's own format
crates/core/runtime    core assembled: the `Runtime` builder components register onto
crates/core/cli        the command line, format-agnostic; the shipped binary hands it a runtime
crates/core/testkit    the scaffolding the end-to-end batteries share
crates/github/frontend   the GitHub Actions frontend: lowers workflow files to the IR
crates/github/actions    the GitHub Actions step kinds: run, action, checkout
crates/github/objects    the ObjectService: cache, artifacts, and tool-cache storage
crates/github/acceptance the acceptance battery: corpus harness and end-to-end runs
crates/fabro/frontend    the Fabro frontend: lowers Graphviz DOT workflows to the IR
crates/fabro/steps       the Fabro step kinds: command, wait, human, agent over ACP or native Pebble, nested workflow, and the stub registry
crates/fabro/acceptance  the Fabro battery: corpus harness, runs under stubs, the Fabro oracle, real steps, the compatibility contract and bundle manifest
crates/petri/lib       the distribution: core plus every component
crates/petri/cli       the shipped `petri` binary: the distribution handed to core's CLI
```

Packages are named `petri-*` and arrows point down. Core never depends on a component; components
depend on core and never on each other; only the distribution names them all.
`crates/petri/lib/tests/layering.rs` enforces this from `cargo metadata`. The next
format (CircleCI, RWX) is a `crates/<component>/` directory and one
registration in the distribution, as `crates/fabro/` is.

A node fires when its **join policy** is satisfied by incoming tokens. On completion
its **routing policy** emits tokens on outgoing edges. Routing is an AND of XORs:
each select group emits at most one token, and groups emit concurrently. The default
is selection, so fan-out is never implicit — it takes writing more than one group.

```rust
use engine::{EngineStart, EngineState, Event, apply};

let mut state = EngineState::new(graph);
let (state, commands) = apply(state, Event::ExecutionStarted(EngineStart::default()));
// run the commands, feed the results back as events
```

## Where the design lands in the code

| Design section | Code |
|---|---|
| §2 routing, AND-of-XOR | `ir::graph::{Routing, RoutingGroup, SelectionPolicy, Guard, Fallthrough}` |
| §3 core types | `ir::graph`, `ir::ids` |
| §3 expressions | `ir::expr` — `Expr`, `ExprTable`, `Context`, `eval` |
| §4 runtime types | `ir::flow` — `Token`, `Outcome`, `Status`, `Metrics` |
| §4 firing rule | `engine::apply::try_fire` |
| §4 quiescence | `EngineState::is_quiescent`, `apply::finish_if_quiescent` |
| §5 engine interface | `engine::event` — `Event`, `Command`; `engine::apply::apply` |
| §5 event log | `engine::log` — v7, with durable admission and routing decisions |
| §5 replay | `engine::replay` — `verify_replay` is the determinism canary |
| §4 retries | `ir::RetryPolicy`, `engine::apply::on_retry_elapsed` |
| §4 run context | `ir::RunContext`, `engine::state::EngineState::record_outcome` |
| §4 expression environment | `ir::EvalEnv` — one way to see upstream state |
| exec §1 layering | `executor::Executor` / `ExecEnv`, `steps::StepRunner`, `driver::Driver` |
| exec §2 driver loop | `driver::run` — one channel, so arrival order is the total order |
| exec §3 process step | `steps::process`, `steps::outputs` |
| exec §4 cancellation | `steps::process::ladder`, `driver::Driver::on_hard_deadline` |
| exec §5 environments | `executor::scope` (the interface), `executor_sandbox::{HostExecutor, SandboxExecutor, RoutingExecutor}` |
| exec §6 secrets | `executor::secrets`, `driver::LogSink` |
| execution hierarchy | `execution::{Coordinator, InvocationId, ExecutionId, InvocationClient}` |
| standalone persistence | `execution::{CoordinatorStore, ResourceStore}`, one engine log per execution |
| §5a cancel scopes | `engine::state::CancelScope`, `apply::on_cancel`, `apply::on_kill` |
| §6 HIR → plan lowering | `engine::context::resolve_config`, `apply::expand` |
| §6 splice semantics | `engine::event::SubgraphSplice`, `apply::on_node_expanded` |
| §6a sequential `for_each` | `ir::desugar::sequential_for_each` — no new IR, just a cycle |
| GHA frontend mapping | `crates/core/engine/tests/gha.rs` |
| §7 validation invariants | `ir::validate` — `check` for errors and warnings, `validate` for errors alone |
| §8 reserved seams | `StepKind::fingerprint`, `Control`, `Command::{Acquire,Release}Scope`, `EventLog::version` |

## Reading the tests

The test suites are the executable form of the design document.

```
crates/core/engine/tests/routing.rs          §2 selection, fan-out, OR-split, guards, preconditions
crates/core/engine/tests/joins.rs            §3 All / Any / Quorum, generations, entry seeding
crates/core/engine/tests/loops.rs            §6a sequential for_each, back edges, firing budgets
crates/core/engine/tests/expansion.rs        §6 parallel for_each, collectors, max_parallel, fail_fast
crates/core/engine/tests/cancellation.rs     §5a cancel scopes, both tiers: routing, run_on_cancel, kill
crates/core/engine/tests/scopes.rs           §3 resource scopes, env, acquire/release
crates/core/engine/tests/gha.rs              the GHA mapping table, end to end
crates/core/engine/tests/retries.rs          handoff §1 attempts, backoff, exhaustion
crates/core/engine/tests/run_context.rs      handoff §2 nodes.* / kv.*, goal gates, merge order
crates/core/engine/tests/partial_success.rs  handoff §4 soft failure, is_success_like
crates/core/engine/tests/seeding.rs          seed edges for entry nodes and clone entries
crates/core/engine/tests/resolved_firing.rs  the executor boundary: no unresolved ExprId crosses it
crates/core/engine/tests/event_log.rs        §5 logging, determinism, serde round-trip, §8 seams
crates/core/ir/tests/validation.rs           §7, invariant by invariant
crates/core/ir/tests/expressions.rs          the expression language

crates/core/driver/tests/e2e.rs           exec §7 1-2: native loop and GHA-shaped, real processes
crates/core/driver/tests/cancellation.rs  exec §7 3,4,5,9: the ladder and the hard deadline
crates/core/driver/tests/kill.rs          §10 two-tier stop: cleanup after cancel, cleanup grace, KillRequested
crates/core/driver/tests/release.rs       §10 sentinel-pinned groups: release kills, reaps, then only observes
crates/core/driver/tests/timeout.rs       exec §7 6: timeouts, and the race under replay
crates/core/driver/tests/environments.rs  exec §7 7,10: acquire failure and retention
crates/core/driver/tests/secrets.rs       exec §7 8: masking, and what reaches the log
crates/core/driver/tests/docker.rs        exec §7 3,10 Docker halves; skipped without a daemon

crates/core/frontend/tests/expr_grammar.rs   frontend §7 1-2: grammar, precedence, coercion, fuzz, table gate
crates/github/frontend/tests/lowering.rs     frontend §7 3-6, the pure half: what lowering produces
crates/core/frontend-native/tests/native.rs  frontend §7 7, the pure half; invariant 8 hint
crates/github/acceptance/tests/gha_e2e.rs    frontend §7 3-6, run for real: truth table, matrix, composites
crates/core/runtime/tests/native_e2e.rs      frontend §7 7: the cycle, run end to end
crates/github/acceptance/tests/harness.rs    frontend §7 8: every corpus workflow lowers or is rejected specifically
crates/github/acceptance/tests/e2e.rs        frontend §7 9: two real corpus workflows run on the executor

crates/fabro/frontend/tests/lowering.rs      Fabro plan §7 3: tiers, failure policies, goal gates, parallel, budgets, rejections
crates/fabro/frontend/tests/conditions.rs    Fabro plan §7 4: the condition grammar against Fabro's semantics
crates/fabro/frontend/tests/fuzz.rs          Fabro plan §7 1: arbitrary text never panics the parser
crates/fabro/acceptance/tests/harness.rs     Fabro plan §7 2: every corpus file lowers or is rejected specifically; REPORT.md
crates/fabro/acceptance/tests/runs.rs        Fabro plan §7 5: every lowered corpus file runs under stubs; RUNS.md
crates/fabro/acceptance/tests/routing.rs     Fabro plan §7 3, 5, 7: the scripted battery, checked against the Fabro oracle
crates/fabro/acceptance/tests/workflow.rs    Fabro plan §5.2: nested workflows through the coordinator
crates/fabro/acceptance/tests/e2e.rs         Fabro plan §7 6: gh-list, hello, a for_each fan-out, random selection, end to end
crates/petri/cli/tests/fabro_cli.rs          Fabro plan §6: `petri run --auto-approve` answers a human gate
crates/petri/cli/tests/inspect_cli.rs        black box phase 2: `petri inspect` over finished, restarted, failed, cancelled and damaged run dirs
crates/core/execution/tests/inspect.rs      black box phase 2: `inspect_run` reconstruction, retries, children, torn and corrupt logs
crates/petri/cli/tests/fabro_blackbox.rs     the Fabro black box battery: the shipped binary against provider twins on loopback, scripted interviews, retention, the readiness milestone A smoke run with no `fabro` on PATH (`milestone_a_smoke_run_without_fabro_on_path`); every read of a finished run goes through `petri inspect --json`
crates/petri/lib/tests/interview.rs          the interview dispatcher on the standalone host: parallel gates, sensitive masking, a failing interviewer, cancellation, occurrence across a loop, nested invocation paths, the re-ask, expiry
crates/petri/lib/tests/controls.rs           readiness item 6: the circuit breaker across restarts and resume, node visit totals across `loop_restart`, the stall watchdog, pause, cancel while paused, steering
crates/core/driver/tests/interview_budget.rs readiness item 6 on a controlled clock: own-stage and sibling waits, overlapping questions, active work after a wait, handler-managed nodes, cancellation during a wait, a fresh budget on redispatch
crates/petri/lib/tests/embedding.rs          readiness item 7: a Fabro workflow without adapters, then with fake adapters (pause, skip, block, prepared results, route override, fatal and best-effort transitions, a hook service); the timeline reconstructed from public events; slow, failing and recovering consumers
crates/fabro/steps/tests/steps.rs            Fabro plan §5.2, §6: command, wait, human answered through deliver
crates/fabro/steps/tests/agent.rs            Fabro plan §7 6: the agent step against Fabro's fake ACP agent
crates/petri/lib/tests/fabro_dependencies.rs  readiness item 1: no Fabro crate anywhere in Petri's dependency graph
crates/petri/cli/tests/standalone.rs          readiness item 1: the binary runs a Fabro workflow with no `fabro` on PATH
```

### The Fabro compatibility contract

`crates/fabro/acceptance/CONTRACT.md` freezes the Fabro reference revision
(`crates/fabro/corpus-pin.txt`), the required workflow bundles
(`crates/fabro/acceptance/bundles.lock.json`, materialized and verified by
`scripts/corpus-fetch-fabro-bundles.sh`), the feature matrix for every
construct and `workflow.toml` option those bundles use, and the accepted
differences. Petri production code never links or launches Fabro. The parity
harness in `crates/fabro/oracle/harness/` builds the pinned `fabro` binary from
the fetched corpus and drives it as a subprocess through its CLI and server
API; `scripts/oracle-regenerate.sh` uses it to refresh the oracle fixtures, and
the routing test rejects a fixture from any other Fabro revision.

Host and container scopes use the `sandbox-driver-host` and
`sandbox-driver-docker` plugins. Petri launches them and communicates over
JSON-RPC; no provider crate is linked in. `mise run plugins:build` installs
both from the pinned revision under `target/plugins/bin`. The development
and test tasks select those binaries. Docker tests skip when no daemon is
reachable; Host execution requires its plugin and no daemon.

The Host plugin keeps private records under `<run_dir>/host-registry`.
It owns each lease's workspace and process groups. After a plugin crash,
recovery fences old work before reuse and never signals saved process ids.
Retention keeps or deletes the workspace with its sandbox; `petri sandbox
prune` deletes retained Host resources through a fresh plugin.

Host jobs inherit `PATH`, `HOME`, `USER`, `SHELL`, `LANG`, `TERM`,
`TMPDIR`, `GOPATH`, `CARGO_HOME`, and `NVM_DIR`. Pass other values and secrets
explicitly through workflow environment settings.

`petri run --backend host|docker|daytona` selects the execution backend.
Host is the default. Docker runs process jobs in a pinned slim runner image.
Daytona runs them in a VM with nested Docker; container jobs, services, and
Docker actions stay inside that VM. `--runner-image LABEL=IMAGE` overrides a
runner label. Docker defaults cover Ubuntu 22.04, 24.04, and 26.04. Daytona's
published Docker-in-Docker default covers Ubuntu 24.04 and `ubuntu-latest`;
other labels need an override. Unknown or conflicting labels fail at acquire.

Daytona reads its credentials from `DAYTONA_API_KEY` or `DAYTONA_JWT_TOKEN`
and the SDK's endpoint and organization variables. `--daytona-cpus`,
`--daytona-memory-mb`, and `--daytona-disk-mb` default to 2, 4096, and 20480.
Runner snapshots are named by image and resources, prepared once per run,
and reused across runs. Snapshot preparation has a 15-minute deadline.
Petri disables automatic VM stop, pause, deletion, and TTL timers; lease
release and prune control cleanup. Shared runner snapshots remain available.

Daytona has no inferred route to Petri's ObjectService. Ordinary JavaScript
and Docker actions still run, with the service variables omitted. Artifact
and cache actions need `PETRI_SANDBOX_DAYTONA_HOST_ADDRESS` set to a name or
address reachable from the VM. Local transport and adapter tests pass;
the ignored live Daytona workflow test requires credentials and has not yet
been run against the hosted preview service.

Release archives bundle the Docker, Host, and Daytona plugin executables from
the pinned revision. Petri embeds their SHA-256 digests at release build time.
`scripts/release-verify.sh` checks the archive, runs Host and Docker workflows
without development mode, and verifies rejection of modified plugins. The CI,
nightly, and release jobs use separate read-only deploy keys for sandbox-driver,
Pebble, and lithos-llm from the `sandbox-driver-read` environment. See
[private dependency setup](DEVELOPING.md#private-dependencies).

Fabro agent nodes can use Pebble directly as a Rust library. Set
`backend="api"` and `model="provider/model"` on the node, or set graph
`backend` and `default_model`. The `petri` distribution reads provider
credentials from its environment. ACP remains the default. See
[native Pebble configuration](crates/fabro/FORMAT.md#native-pebble) for scope
requirements, client injection, events, and accounting.

`petri inspect --run-dir <dir> [--json]` reconstructs a run from its run
directory alone: `run.json`, `coordinator.jsonl`, the registered graphs, and
each execution's `events.jsonl`, replayed through the engine. It never starts a
step, contacts a provider, takes the run lease, or writes under the run
directory, so it works after the process exits, after the source workflow is
gone, and while another process holds the run. `--json` prints a versioned
document (`inspect_format_version`) with the run status, the root invocation
and its final execution, every invocation with its parent call and children,
every execution in order with its own derived run context (`kv` plus the
node-instance records: status, output, generation, attempts), the final firing
history, every attempt including retries, every applied route, and the
interview receipt (`interviews`, from `<run-dir>/interviews.json`) when the run
had an interviewer. Secret values stay masked or as `$secret` references. An interrupted run is reported
as incomplete with the reasons (exit 1); a corrupt, truncated-then-terminated,
diverging, or unsupported-version log is an error (exit 2), never a final
snapshot. The field contract is
[`crates/core/execution/INSPECT.md`](crates/core/execution/INSPECT.md).

`mise run test:remote` transfers an artifact through a separate Docker daemon
with no host filesystem mounts. `mise run check` includes this test. Set
`PETRI_REQUIRE_DOCKER=1` to require Docker instead of skipping unavailable tests.

`mise run test` runs all of them with Nextest, then runs the maintained doctests
with Cargo.

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

These began as implementation notes and are now rules in `engine-spec.md`, because
each follows from something the design already said rather than from a choice the code
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
   (`crates/core/engine/tests/seeding.rs`).

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

30. **Cancelled outcomes route, and stopping has two tiers.** Frontend finding 3:
    GitHub runs `if: always()` and `if: cancelled()` cleanup after a cancel, and
    the engine could not, ever — the spec had cancellation drop the scope's
    tokens, making `Cancelled` the one terminal status that was recorded but
    never routed. Spec §5 now says: a cancelled firing's outcome routes like any
    other, pending tokens survive, and what stops work from restarting is
    **structural** — a node in a cancelled scope, or fed by a token from a firing
    that recorded `Cancelled`, completes `Cancelled` without evaluating anything
    unless it carries `Node.run_on_cancel`. No gate anywhere has to be right for
    cancellation to be safe (review showed the implicit gates could not be: a
    job's first step, a dependent whose needs finished pre-cancel, and a
    `max_parallel`-deferred clone all pass them). The engine's *previous* cancel
    behavior was renamed rather than deleted: `Event::KillRequested` is the
    forced tier — tokens drop, nothing routes, nothing is admitted,
    `Control::Kill` goes straight to `SIGKILL` — and it is in the log, so replay
    gets the hard stop for free and the mode of stopping is recorded, never
    inferred. The driver wires the tiers as cleanup grace and second-cancel; the
    host executor grew sentinel-pinned process groups so release can end
    stragglers without ever signalling a recycled pgid. Log v2 → v3. (The GHA
    frontend has since stopped choosing where the flag lands from condition
    text: it sets `run_on_cancel` on every node it emits — matrix expansion
    heads excepted — and lets each step's gate evaluate the condition against
    the post-cancel state. The engine's structural rule is unchanged; the flag
    just stopped being an admission *decision* in that frontend.)

## Frontends (package 03)

Both frontends are pure: text in, `Graph` and diagnostics out. One parser reads the
`${{ }}` grammar; two lowerings give it meaning. The GHA lowering maps `==` onto
`loose_eq`, `&&` onto a value-returning conditional, and every GitHub function onto a
table entry — there is no second evaluator, and a property test asserts that whatever
parses lowers only onto `ir::expr::BUILTINS`. The native lowering maps the same syntax
onto the engine's own `Eq` / `And` / `Not`.

### The corpus

316 workflows from 21 repositories with `.github/workflows` (22 fetched). Two
instruments measure them, both regenerated by the harness and committed beside
the corpus pins:

- **`crates/github/corpus/REPORT.md`** — the *lowering* bar: every workflow
  either lowers or is rejected with a specific `unsupported.*` code — zero
  panics, zero generic errors. Windows/macOS workflows, callee-only reusable
  files, and broken-upstream references leave the denominator by policy.
- **`crates/github/corpus/RUNS.md`** — the *run-time* bar: every in-scope
  workflow that lowers, run end to end (`--ignored`, network + Docker) with
  `run:` scripts stubbed to `true` and `uses:` steps real, every host scope
  rewritten to a pinned [sandbox-images](https://github.com/lithoscomputer/sandbox-images)
  runner container so corpus code never executes on the host. First-failure
  classes are ranked, with server-coupled failures (OIDC, repository secrets,
  SaaS backends, credentials) kept apart from real runtime-tier gaps — the
  same denominator discipline, applied at run time.

The corpus is fetched, not committed: the workflows are their authors' property, so
`scripts/corpus-fetch.sh` downloads them at the commits pinned in
`crates/github/corpus-pins.txt`, and `crates/github/corpus/` is gitignored apart from
the two reports. Each fetched repo gets a `PROVENANCE.md` with its commit and licence.
The corpus tests skip when the corpus is absent, and `PETRI_REQUIRE_CORPUS=1` — which
CI sets — turns that skip into a failure.

Corpus workflows also run end to end in the acceptance battery
(`crates/github/acceptance/tests/e2e.rs`), with a stub `gh` on `PATH` so nothing
reaches GitHub's API; the deterministic runtime-tier batteries (artifacts across
jobs, the cache round-trip, local checkout, the tool cache) live beside it.

### The runtime tier

What a lowered workflow finds at run time, standing in for the world GitHub
provides around a job:

- **The ObjectService** (`crates/github/objects`): one process per run speaking
  the results backend the 2026 toolkit converged on — twirp `ArtifactService`
  and `CacheService` under one listener, one minted token, one signed-URL blob
  implementation. Artifacts are run-scoped under the run dir; cache entries
  live in the host's persistent store (`$PETRI_STORE`, default
  `~/.cache/petri/store`, 10 GiB LRU) and outlive the run. The token is also
  the auth: the exact bearer string, salted per run, so binding beyond
  loopback for container reachability (`host.docker.internal`, guaranteed via
  `--add-host`) never leaves an open write endpoint.
- **Local checkout**: a supportable `actions/checkout` call substitutes at
  lowering for the `github/checkout` step, which materializes the workspace
  from the run's own repository — committed HEAD plus the uncommitted tracked
  diff and untracked-but-not-ignored files, `.git` and mode bits included —
  offline and token-less. Run identity is honest too: `default_params` fills
  `github.sha`/`ref` from the checkout's HEAD (run parameters, so lowering and
  replay are untouched).
- **The tool cache**: host jobs point `RUNNER_TOOL_CACHE` at
  `<store>/toolcache/<os>`; a runner image that ships a populated
  `/opt/hostedtoolcache` keeps its own.

**The GitHub API stance (v1, decided 2026-08-28): petri neither proxies nor
blocks it.** `github-script` and `gh` steps hit the real API with the ambient
token when one exists (`$GITHUB_TOKEN`, else a logged-in `gh`); without one,
public reads work and authenticated calls fail routably. **A local run with a
real token can mutate — comment, tag, release — exactly as the workflow says.**
Prefer a fine-grained read-only token when running workflows you did not
write. The corpus sweep runs token-less by policy, so corpus code can never
act with your identity. A default-deny proxy stays future work if real demand
appears.

### Findings for the spec

These are places a real workflow needs something the engine or the expression table
cannot express. They are reported, not worked around; each is rejected with the code
in brackets.

1. **There was no per-run input channel.** The `github`, `vars` and `runner` contexts
   are values a host knows and the graph does not. I added `Graph.params`: a read-only
   map merged into every firing's statics at the lowest precedence, filled in by the
   host before the run. It lives on the graph rather than in a `RunStarted` payload so
   the graph a run used is self-describing and replay needs nothing more. This is an
   IR extension, made because nothing in the package works without it; if it should
   live elsewhere, it is a small swap.

2. **`hashFiles()` cannot be a total builtin** [`unsupported.expression.hashFiles`].
   It reads the workspace at run time. Resolved for step config: a literal-pattern
   call in `run:`, `env:` or `with:` lowers to a sentinel — the second permitted
   non-literal across the executor boundary, like `$secret` — which the GitHub step
   kinds replace at spawn with the hash, computed in the job environment
   (`crates/github/actions/src/hashfiles.rs`). Resolved for step-level conditions
   too: an `if:` lowers to a config gate the step evaluates at spawn, and the
   sentinel rides in the gate's literals (the React sizebot shape,
   `if: hashFiles(...) != ''`, runs). In a position the engine evaluates — a job
   `if:`, an output, a matrix, or under another function in a condition — it
   stays rejected, and so do computed patterns.

3. **Nothing ran after a cancel** — resolved; see departure 30 and spec §5.
   `if: cancelled()` and `if: always()` cleanup can now run: cancelled outcomes
   route, admission is the structural `Node.run_on_cancel` flag, and the forced
   tier (`KillRequested`) keeps the old stop-everything semantics under its own
   logged event. The pinning test flipped into
   `cancelled_steps_run_after_a_cancel`.

4. **One scope per job, so no per-leg runtime** [`unsupported.runs_on.expression`].
   `runs-on: ${{ matrix.os }}` is the single most common reason a matrix job is
   rejected: 47 workflows. Each leg would need its own environment. Either scopes
   become per-clone, or the frontend splits a matrix over `runs-on` into one job per
   OS at lowering time.

5. **Secrets in expressions** [`unsupported.secrets.expression`]. `if: ${{ secrets.X
   != '' }}` and `${{ secrets.A || secrets.B }}` are real idioms — 12 workflows. The
   engine keeps secrets out of `EvalEnv` by construction, and the docs' context
   matrix gives conditions no `secrets` context either, so the rejection stands
   (we adhere to the docs even where GitHub accidentally accepts more). The
   sanctioned form works now: pass the secret through an environment variable and
   test `env.NAME` in the step's `if:` — the gate resolves it at spawn,
   step-side, so no value ever enters an expression the engine evaluates. A job
   output that is a whole-value secret is dropped with a warning
   (`ignored.secret_output`), as GitHub drops it.

6. **Background steps are a job-local DAG** — resolved. `background: true`
   fans a `run:` or `uses:` step out from the foreground chain. `wait`,
   `wait-all`, and `parallel` add explicit joins, and an implicit wait joins
   remaining work before post-action cleanup. A private environment channel
   defers outputs, `GITHUB_ENV`, and `GITHUB_PATH` until the join publishes the
   step. `cancel: id` stops that branch through an engine cancellation group,
   joins it, and publishes its effects. The target records `cancelled`; the
   cancel control succeeds and the foreground job continues. Matrix legs and
   deferred composite actions preserve their cancellation boundaries.

7. **Action manifests resolve when their steps run** — resolved. Remote
   references still pin at lowering, but local and pinned manifests use one
   run-time resolver path. `uses: ./localClone` and
   `uses: ./node/.github/actions/x` can name directories created by an earlier
   step. A remote composite or called workflow can also use a `./` action from
   its own pinned repository.

8. **`GITHUB_ENV` and `GITHUB_PATH`** are job-scoped mutable env across steps.
   Resolved: the `github/run` and `github/action` step kinds (`crates/github/actions`)
   accumulate them in files in the job's workspace and apply them to every later
   step, with no core support — see `.ai/plans/github-actions-runner.md`.

9. **The positional YAML reader has two gaps** [`unsupported.yaml.anchors`,
   `unsupported.yaml.multiline_flow`]: no anchors or aliases, and a multi-line
   `[ … ]` value followed by a dedent is misread. Three cpython workflows. Both are
   library limitations, named as such.

Custom shells are resolved: `python`, `pwsh` and any `{0}` template (`bash -el {0}`,
`/usr/bin/env bash {0}`) lower now — the step writes the script to a file and runs
the template over it, as GitHub does; only the Windows-only `cmd` and `powershell`
stay rejected. `concurrency:` is ignored with a warning (`ignored.concurrency`):
cross-run mutual exclusion has nothing to race in a single local run, and its
cross-run semantics stay with the multi-run driver layer (D2). Windows and macOS
runners are out of scope (`runs_on.windows`, `runs_on.macos`): the local executor
emulates Linux runners only. The declared support matrix is
`crates/github/SUPPORT.md`, held in sync with the corpus report by a harness test.
Reusable workflows now run as child invocations, including a call matrix with
a job matrix inside the child. Remaining `runs-on` expression limits are listed
in the support matrix.

### What the core may know

An audit after package 03 asked whether GitHub-specific code had leaked into the
core, since every frontend added later — CircleCI, Buildkite, RWX, Attractor — would
otherwise leave its own residue there. The rule that came out of it:

> A primitive may enter `ir`/`engine` when it is generic — a thing several formats
> would plausibly want, described on its own terms. Nothing uniquely one format's may.
> Format semantics are *compositions* of primitives, and they live in that format's
> crate.

What the audit found and changed:

- **One real leak:** the `matrix_combinations` builtin carried GitHub's
  `include`/`exclude` merging rule. Replaced by generic combinators (above); the
  GitHub rule is spelled out in `frontend_gha::expr_lower::matrix_legs`, and GitHub's
  documented examples are tested there, through the engine's evaluator.
- **Wrong layer:** the GHA expression lowering (`gha`, `gha_function`,
  `GHA_FUNCTIONS`) lived in the *shared* `frontend` crate. Moved to
  `frontend-gha`. The shared crate now owns syntax and the strict lowering only.
- **Framing:** `loose.rs` and the `loose_*` summaries described themselves as
  "GitHub's rules". The semantics are the JavaScript-family ones; they are now
  described that way. `filter_field` became `pluck_present`; `gha_format` became
  `positional_format`.
- **Kept, by judgment:** `loose_string` renders a container as its type name
  (`Array`, `Object`), which is exactly GitHub's output. A loose string coercion needs
  *some* rule for containers, and type-name makes a value that should have been a
  scalar visible instead of a JSON blob silently reaching a command line. Documented
  on those terms.
- **Runtime layer, GitHub-influenced by design:** the process step's outputs file
  accepts the `key<<DELIM` heredoc form, a superset of `$GITHUB_OUTPUT`'s format so
  that variable can be a plain alias. A compatibility choice inside one step kind,
  documented as such; the engine does not know it exists.
- **Clean:** `Status`, `RetryPolicy`, `Expansion`, `RunContext`, `Graph.params`,
  `NoopStep`, `soft_fail`, the driver, the executor. Comments that cited GitHub as
  the *reason* for a rule were reworded to state the rule.

### What the executor interface may know

The same audit, for executors. There are two today — host and Docker — and there
will be more; a cloud executor most likely. The rule mirrors the core's:

> The `executor` crate is the interface. It names no executor, and neither does
> anything above it: the IR describes the *kind* of environment a scope needs, step
> kinds reach the workspace only through the interface, and the driver translates.
> Each executor is its own crate, and adding one is adding a crate, not editing the
> interface.

What the audit found and changed:

- **`RuntimeTarget::Docker { image, args }` named an executor in the IR**, and
  `args` were Docker CLI flags. It is `RuntimeTarget::Container { image }` now: a
  *kind* of environment several executors can provide. Engine flags, mounts and
  limits are executor configuration, not something the graph carries.
- **The step kind assumed the workspace was on this machine.** The process step
  read and wrote its outputs file with `std::fs` on `ExecEnv::workspace()`, a
  host-visible path — true for a host workspace, false for anything else.
  `workspace()` is gone from the interface; `read_file` and `write_file` take its
  place. The host executor answers from the filesystem; the container executor
  answers through the sandbox's filesystem facet over the plugin wire, because a
  sandbox owns its workspace and nothing on this machine mirrors it. The step
  kind is none the wiser.
- **The shared error and report types had Docker fields.** `EnvError::Docker` is
  `EnvError::Backend { backend, operation, message }`; `ReleaseReport` describes
  what was released and kept as text rather than as `container_removed` /
  `workspace_removed` booleans that would grow a field per executor.
- **Already right when the audit ran:** the executors live outside the interface
  crate (`executor-sandbox` over the provider plugins), with a private teardown record the interface carries
  as an opaque `Teardown` trait object and hands back untouched; the output pump
  lives in the interface crate because every executor needs it and the line cap
  must be decided once.
- **Kept:** `Retention` (keep-on-failure) is generic — anything with a workspace has
  the question. `Sig` is POSIX; a remote executor maps it. Placement labels stay
  opaque in `RuntimeSpec.requirements`, and the set the GHA frontend accepts is
  GitHub's hosted-runner vocabulary, which is GitHub knowledge in the GitHub crate;
  which of those any executor honours is that executor's, and not yet plumbed.

### Departures from the frontend handoff

30. **`Graph.params`** — see finding 1.

31. **Twenty-three new builtins, one gate — and none of them GitHub's.** The
    handoff's mapping rule (no second evaluator) means GitHub's operators and
    functions must become table entries, so the question is what *kind* of entry.
    The rule adopted: a primitive may enter the core when it is generic — loose
    JavaScript-family coercion (`loose_eq`/`lt`/`le`/`gt`/`ge`/`truthy`/`number`/
    `string`), string functions (`contains_ci`, `starts_with`, `ends_with`, `format`,
    `join`, `to_json`, `from_json`), record access (`get_ci`, `values`,
    `pluck_present`, `keys`, `omit`) and record combinators (`cartesian`,
    `reject_where`, `extend_where`). Nothing uniquely GitHub's may. GitHub's matrix
    `include`/`exclude` rule, which an earlier draft had as a `matrix_combinations`
    builtin, is now a *composition* the GHA frontend emits:
    `extend_where(reject_where(cartesian(omit(m, [include, exclude])), m.exclude),
    m.include, keys(axes))` — the engine has combinators, GitHub has a use of them,
    and Buildkite's `adjustments` will compose the same pieces differently. The table
    grew from 19 to 42 and still gates dispatch. See "What the core may know" below.

32. **Coercion follows the runner, not the docs' wording.** GitHub's docs say a string
    coerces "from any legal JSON number format"; the runner uses .NET's
    `AllowLeadingSign | AllowDecimalPoint | AllowExponent`, which also takes `+1`, `01`
    and `1.`. Workflows run against the runner.

33. **`.` on a plain array is `null`, not a filter.** The handoff says "`.` on arrays
    (object-filter semantics)". In the runner, property access maps over elements only
    on a *filtered* array — the result of `*`; on an ordinary array it is `null`.
    `a.list.k` is `null` and `a.list.*.k` is the mapped list, and the test pins both.
    Filtered-ness is tracked statically in the lowering, so no runtime flag exists.

34. **Every GHA property access is `get_ci`.** Contexts are case-insensitive in
    GitHub; `Field` is not. `github.Event_Name` works, at the cost of a call per
    property.

35. **Status functions expand per site, not to the core builtins.** A step's
    `success()` means "no earlier step of this job failed, and the job was not
    cancelled out from under it", built as an expression over the earlier steps'
    `nodes.*` records (plus the `scope_cancelled` static, waived when the job's
    own gate admitted it *after* the cancel — cleanup jobs run their plain steps,
    as GitHub's do) and conjoined with "the job started"; a job's means "every
    needed job's `done` reports success and the run is not cancelled". The core's
    `success()` builtin folds immediate upstream edges, which is not GitHub's
    job-status rule. A step or job `if:` naming no status function gets
    `success() &&` in front, as GitHub does; one that names any status function
    stands alone. Job conditions stay engine preconditions; step conditions ride
    as config gates the step evaluates at spawn (which is what lets them read
    `env.*` — `GITHUB_ENV` appends included — and `hashFiles`), with the same
    expressions inside as `$expr` leaves.

36. **Jobs get `start` and `done` noop nodes.** `start` is the gate (`needs` + `if:`);
    `done` folds the job — `{ result, outputs }` — from the payload the last step's
    edge carries, one per matrix leg, and fans out to dependents. Every `needs.J.*`
    reads `J/done`. `NoopStep` is a step kind whose output is its resolved config,
    which makes a structural node also a way to compute a value.

37. **`kv` is unused by the GHA lowering.** Job outputs ride the summary payload
    instead; there was no need for run-scoped state.

38. **`secrets.misplaced` became `unsupported.secrets.expression`.** The handoff
    asked for an Error diagnostic; this is one, coded as a rejection because it is an
    engine property by construction, not a user mistake — and so it counts as
    specific in the corpus.

39. **`permissions:` and `on:` are accepted.** Both describe the GitHub side —
    token scope, triggers — and change nothing about how the graph runs locally, so
    they are not "parse and ignore" in the sense the rule forbids. `permissions` gets
    an `ignored.permissions` warning so nobody wonders; `on:` is the host's business.

40. **Fuzzing is proptest, not cargo-fuzz.** Random text, random bytes lossily
    decoded, and random glued syntax fragments, 600 cases each, assert the parser
    never panics. cargo-fuzz needs nightly; this runs in the ordinary suite.

41. **`serde_json` has `preserve_order` on, workspace-wide.** Matrix axes,
    `include` order and outputs follow document order in GitHub; a sorted map
    silently reordered them (the matrix test caught it).

42. **The driver now records `StepStarted`.** It never had, only the test harness
    did — so a real run's log had no start events. A package 02 gap, found by the first
    test that read a real driver's log for them.

43. **Reusable-workflow callers are not asked for `runs-on`**, `strategy:` without
    `matrix:` is legal, `uses: ./` is the repository root, and current GitHub-hosted
    labels (`ubuntu-slim`, `ubuntu-26.04`, `ubuntu-24.04-arm`, `macos-15`) are known.
    All four came from the corpus; each cost a real workflow a generic error before.
    (macOS labels have since moved out of scope — `runs_on.macos`; the local executor
    emulates Linux runners only.)

44. **`KNOWN_RUNS_ON` lives in the frontend.** The handoff puts label mapping in the
    executor; `petri check` still needs to reject unknown labels without one, so the
    frontend carries the list the local executor honours. Third-party runner labels
    (`depot-*`, `namespace-profile-*`, `*-16-core-*`) are rejected per label: 23
    workflows.

45. **Composite outputs are expression substitution.** `steps.<caller>.outputs.<n>`
    resolves to the action's declared `value:` expression, lowered in the composite's
    own site — no node, no record.

46. **`GITHUB_OUTPUT` is an alias**, via `ProcessConfig.output_env_aliases`. A `run:`
    step that appends to it lands in the outputs file without a shim.

## Testing notes

Three things this package taught, kept because they generalise:

- **Assert the duration, not just the result.** A `docker exec` that returns early
  looks exactly like a step that finished fast. Pinning elapsed time alongside the
  exit status in `docker_wait_follows_the_step_not_the_client` is what makes that
  regression loud instead of silent.
- **A cancellation test that only checks the process is gone will pass on teardown
  alone.** Releasing the scope kills everything either way, so the test has to pin
  *how* the step ended — which signal, which escalation — not merely that it did.
- **Docker tests skip without a plugin and a daemon, and `PETRI_REQUIRE_DOCKER`
  turns that skip into a failure.** CI sets it on the Linux job. A silently skipped
  acceptance battery is indistinguishable from a passing one, and that job exists
  precisely to say the battery ran.
- **A container test reads the workspace through the sandbox, never through a
  host path.** The workspace lives in the sandbox's own volume. A test checks a
  file the step wrote with `ExecEnv::read_file`, or with `docker exec` through
  `testkit::container_read` when it has no environment handle; a heartbeat that
  must be seen to stop is measured by a step inside the sandbox and reported in
  its output.

## Not built

Out of scope per executor §0, and not built: GHA action shims and the JS action host,
artifact and cache stores, remote or distributed executors, Windows, service
containers, and cpu/memory limits. The `LogSink` writes to the run directory and
optionally stdout; richer sinks come later.

Deliberately out of scope, per the core handoff: cross-run concurrency groups
(D2 — the engine carries no concurrency semantics; the GHA frontend ignores
`concurrency:` with a warning, a single local run having nothing to race), and
placement *semantics* for `RuntimeSpec.requirements` (D3 — the labels are
carried, uninterpreted).

Still v2 in the design document: content caching, remote scope placement, and
`Control::{Pause, Steer, Approve}`. The seams are in place — the log is versioned and
rejects old versions cleanly, `EngineState` serializes whole, `StepKind::fingerprint`
defaults to `None`, and `Control` is `#[non_exhaustive]` (`Kill` was its first
addition). Replay has landed; `engine::verify_replay` is the determinism canary.

## The terminal path

`petri run <workflow>` is the standalone runner: one invocation loads,
validates and executes one root workflow with no server, database or UI. It
prints the run dir first, then every step's output on stderr as
`[<node>#<firing>] <line>` so interleaved output from parallel stages stays
attributable (each line is masked and bounded; `--quiet` turns the echo off),
then one `<status> <node>` line per finished node, the retained workspace
paths, and `run: <status>`. The exit code is 0 for success, 1 for a failed or
cancelled run, 2 for a usage error, 3 for a host error, and 4 when the run
finished but its interview did not go as scripted.

**Answering questions.** A `fabro/human` gate, and a native agent's question
tool, ask through the core `Question` event; the host's `Interviewer` answers
(`execution::Interviewer`, an open trait a product host implements too). The
CLI ships three, one per option, and they exclude one another:

- `--interactive` prints each question on stderr and reads one line from
  stdin. Questions from parallel stages are serialized; a `yes_no` or
  `confirmation` gate takes Enter as its default; a `multi_select` gate takes
  comma-separated keys; a `freeform` gate takes text. EOF or three invalid
  answers fail the gate closed.
- `--auto-approve` takes the default choice, or empty text.
- `--interview-script <file>` answers from a versioned JSON script and fails
  the run when a question matches no entry, matches several, exhausts an
  entry's `count`, or a required entry goes unused. It never falls back to the
  terminal or to auto-approval.

An interview script:

```json
{
  "version": 1,
  "entries": [
    {
      "id": "ship-it",
      "match": { "node": "gate", "kind": "yes_no", "options": ["Y", "N"] },
      "count": 1,
      "action": { "kind": "choice", "value": "Y" }
    }
  ]
}
```

`match` fields (all optional, all must hold): `node`, `invocation_path` (`/`
for the root, `/<slot>` per nested call), `occurrence` (which distinct question
of that node, 1-based), `ask` (which time the same question was asked, after a
rejected answer), `kind`, `text`, `text_contains`, `options` (the offered keys
in order), `default`, `freeform`, `sensitive`, `reference_url_contains` (a
`review_target` gate's URL). Actions: `choice` (`value`), `choices`
(`values`, for `multi_select`; Fabro's `multi_selected` `option_keys`), `text`
(`value`), `negative` (the `N`/`no` option), `invalid` (`value`, sent as a
choice the step must reject; the re-ask matches `"ask": 2`), `cancel`,
`withhold` (no reply until the question is cancelled, so a gate with a
`timeout` expires into its `human.default_choice` or Fabro's retry outcome).
`delay_ms` waits before acting; `required: false` lets an entry go unused.

The terminal shows a `review_target` gate's reference as `review: <label>
<url>` and a gate's answer deadline as `(answer within 90s)`. Steering is not
answering: a control line (below) never consumes a pending question's answer.

**Controls.** `--control <file>` tails a file for run controls while the run is
live, one per appended line: `pause` holds every attempt not yet admitted
(running work continues, and Ctrl-C still cancels), `unpause` releases them,
`steer <node> <text>` delivers guidance to the named stage's live firing (an
agent queues it for its session; a human gate ignores it), and `cancel`
cancels the run (a second `cancel` reaches the kill tier). Each line's effect
is reported as `control: ...` on stderr; a line that is not a command or names
a stage that is not running is reported and skipped. An embedded host drives
the same `execution::controls::ControlService`.

**Policies.** A Fabro graph's `stall_timeout` (default 30 minutes) cancels a
run that emits no execution event for that long; a pending question parks the
clock, and the terminal prints `stall watchdog: no execution activity for N
s`. Its `loop_restart_signature_limit` (default 3) fails a run whose node
repeats one deterministic failure that many times and blocks a `loop_restart`
edge taken by anything but a transient failure. See `crates/fabro/FORMAT.md`,
"Watchdog and circuit breaker".

**The receipt.** Every run with an interviewer writes
`<run-dir>/interviews.json` (`execution::InterviewReceipt`, version 1): one
record per question with its invocation, execution, firing, attempt, node,
occurrence, ask, question id, kind, text, offered option keys, the review
`reference` and `timeout_ms` when the question had them, the reply
(`answered` with `choice`/`choices`/`text`, `cancelled`, or `failed`), and
how it left (`delivered`, `not_live`, `late`, `shutdown`, `withheld`); the
`errors` list; and under `script`, a scripted interviewer's per-entry
`consumed`/`remaining` counts. A sensitive answer appears only as its
`{"$secret": "answer:<id>"}` reference. A non-empty `errors` list is exit
code 4, whatever the engine status; the persisted run is not rewritten.

**Retention.** `--retain always|on-failure|never` decides what happens to the
run's workspaces at teardown. The default is the workflow format's: Fabro
keeps every workspace (its result is the files), other formats keep a failed
run's and delete a successful run's. The run reports each retained workspace:
a host workspace as its path under the run dir, a container's as its sandbox
and the `petri sandbox prune --run-dir <dir>` command that deletes it.

**Provider configuration.** Native agents (`backend="api"`) use the
distribution's `lithos-llm` client (`petri::llm_client`). Credentials are the
provider library's conventional variables (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, and so on), read per request. `PETRI_LLM_CATALOG` names
one or more catalog TOML files (path-separator delimited) layered over the
built-in catalog, so a test or a gateway deployment redirects a provider:

```toml
schema_version = 1
[providers.openai]
base_url = "http://127.0.0.1:3000"
```

`PETRI_LLM_PROVIDERS` (comma-separated provider ids) narrows which providers
may be routed to at all; a model on any other provider is unavailable. An
unreadable or invalid layer leaves the client unbuilt and every native agent
node failing with `pebble_unconfigured`, rather than reaching a live
endpoint.

## The embedding path

A host that embeds Petri (a platform running Fabro workflows, say) uses the
same `Runtime` the CLI does and adds three things, all Petri-owned types with
no platform vocabulary in them:

- **Events.** `execution::events` is the versioned public event contract
  (`crates/core/execution/EVENTS.md`). An `EventProjector` is an
  `ExecutionObserver` that derives `RunEvent`s from every record and hands
  them to the host's `RunEventSink`, awaited per event so a slow store delays
  and never drops; `replay_run` rebuilds the same events, with the same
  identities, from a run dir after the fact, and `EventProjector::primed`
  attaches at resume. Every event names its run, invocation, execution,
  node (with the frontend's `meta`), firing, visit, attempt and branch role.
- **Awaited extension points.** `Runtime::hooks` installs
  `driver::lifecycle::ExecutionHooks`: `before_attempt` (pause, skip or block
  an attempt), `prepare_result` (adjust the effective result; the original is
  recorded beside it), `after_record`, and `transition` (override the selected
  route, report best-effort problems, or fail advancement). Callbacks see an
  immutable `FiringView` and change the run only through their return values.
  Without hooks the driver's fast path is unchanged.
- **Hook service.** `execution::hooks::HookService` is the one interface a
  hook executor implements (`crates/core/execution/HOOKS.md`); the
  `HookAdapter` is its one caller from workflow points, so the local hook
  executor and a platform's hook service both run each configured hook once.
- **Questions.** `execution::Interviewer` and the `InterviewDispatcher`, as
  on the terminal path.

`crates/petri/lib/tests/embedding.rs` is the worked example: a Fabro workflow
run without adapters, then with fake adapters that exercise every point, with
the timeline, attempts, branches, question, outcomes and accounting
reconstructed from public events alone.

## Local run layout

The standalone host stores one root run as `Run → Invocation → Execution → Firing`.
`coordinator.jsonl` records graph registrations, invocation calls, execution
successors, and final results. Each execution keeps an independent engine log. `petri inspect` reads this layout back.

```text
<run-dir>/
  run.json
  coordinator.jsonl
  graphs/<sha256>.json
  resources/
  invocations/<invocation-id>/
    invocation.json
    executions/<execution-id>/events.jsonl
```

A restart creates a successor execution in the same invocation. It starts at the
selected target with empty context and carried firing budgets. Nested workflow calls
create invocations, not separately managed runs. The coordinator holds an exclusive
lease on `run.json` while it creates or resumes the run.
