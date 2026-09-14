# The Fabro integration handoff

This page is what a host that embeds Petri to run Fabro workflows can rely
on, and the order of the work that remains on the Fabro side. Readiness item
10 (`.ai/plans/fabro-execution-readiness.md`) asked for it; the evidence
behind every claim is `crates/fabro/acceptance/CONTRACT.md` (the support
matrix, the readiness audit) and `crates/core/execution/EVENTS.md` (the event
contract and its coverage matrix).

Petri owns workflow semantics, execution, the durable run record (through
the store seam, the run directory by default), the public event stream and
the extension points. Pebble owns the agent loop.
`lithos-llm` owns provider transport. Fabro owns its platform: Git
checkpoints and branches, the database, the UI and API, publication,
notifications, the vault. Nothing in Petri production code depends on,
links, or launches Fabro (`crates/petri/lib/tests/fabro_dependencies.rs`,
`crates/petri/cli/tests/standalone.rs`); every adapter that calls a Fabro
handler lives in Fabro.

## What a host implements

The optional interfaces are Petri-owned and versioned. A host installs any
subset; the unconfigured path stays the standalone runner.

| Need | Interface | Where |
|---|---|---|
| Build a runtime with the Fabro step kinds | `petri::fabro::register(Runtime::standard().frontend(Fabro::new()))` and the `PebbleClient` capability (`petri::build_llm_client` with the host's `CredentialProvider` and catalog layers) | `crates/petri/lib/src/lib.rs`, `crates/fabro/steps/src/lib.rs` |
| Load and run one workflow | `Runtime::check` (lowering with diagnostics), `execution::host::HostRun`, `host::run_configured`; `RunOptions::run_key` names the run with the host's own id | `crates/core/execution/src/host.rs` |
| The run's durable record | `store::RunStore` and `store::RunLogs` (`crates/core/store`), installed with `Runtime::store`: open a run by key in one access mode (`Create`, `Write` under an `OwnerId`, `Read`), then `append` and `read` records per log (`LogId::Coordinator`, `Resources`, `Execution(id)`) and `put_blob` and `get_blob` by digest. The stored unit is the record, exactly the `record` value of a public event. Petri ships `RunDirStore` (the run directory) and `MemoryRunStore`; a host implements the two traits over its database and runs `testkit::run_store::conformance` against it. See "Two shapes" below | `crates/core/store/src/lib.rs`, `crates/core/testkit/src/run_store.rs` |
| Awaited extension points: admission, result preparation, transition, run end, scope release | `driver::lifecycle::ExecutionHooks`, installed with `Runtime::hooks`; `AdmitAttempt` (`Admit`, `Skip`, `Block`), `PrepareResult` (`Prepared` adjustments with the original evidence kept), `Transition` (`RouteOverride`, best-effort `problems`, a fatal `TransitionError`), `RunFinished`, `ScopeReleased`; notes returned at each point are durable records | `crates/core/driver/src/lifecycle.rs`; proven by `crates/petri/lib/tests/embedding.rs` and `embedding_readiness.rs` |
| The local hook system, or a replacement | `execution::hooks::HookService` behind `HookAdapter` and the `HookServiceHandle` capability; the standalone service is `fabro_steps::hooks::LocalHooks`. Every point, the ones steps ask themselves included (`ScopeReady`, `RunStarted`, the start stage's admission, `ForkStarted`, `ForkCompleted`, the tool boundary of both agent backends), reaches the one service through that handle, so a replacement receives each exactly once (`embedding::a_hook_service_runs_each_hook_once_at_its_point`). A host that installs its own `ExecutionHooks` and still wants `[[run.hooks]]` calls `register` first and wraps `Runtime::installed_hooks()`, forwarding every point, `run_finished` and `scope_released` included (the `EmbeddingHost` in `embedding_readiness.rs` is the pattern) | `crates/core/execution/HOOKS.md` |
| Questions and answers | `execution::interview::{Interviewer, InterviewDispatcher}`; `InterviewRequest` carries the interaction identity (node, firing, occurrence, invocation path), the question type and choices; `InterviewReply::Answered(Answer)`, expiry, cancellation | `crates/core/execution/src/interview.rs` |
| Pause, unpause, steer, cancel | `execution::controls` (the control service; `petri run --control <FILE>` is the terminal transport), `RunHandle` for cancel and kill | `crates/core/execution/src/controls.rs` |
| The public event stream | `execution::events::{EventProjector, RunEventSink, replay_run, replay_since}`; `EVENT_CONTRACT_VERSION`. A public event carries its record unchanged under `record`, plus what Petri derived under `derived`; `execution::events::verify_export` proves the records equal the stored logs and replay, at the end of every run | `crates/core/execution/EVENTS.md` ("Export") |
| Durable inspection of a stored run | `execution::inspect::inspect_run` over a read handle (`inspect_run_dir` and `petri inspect --run-dir --json` over a run directory), `INSPECT_FORMAT_VERSION` | `crates/core/execution/INSPECT.md` |
| Output references and large values | the `OutputStore` capability (`BlobStore`); the default is a local store under `<run_dir>/blobs` writing `blob://sha256/<hex>` | `crates/fabro/steps/src/blobs.rs` |
| Secrets | the `SecretProvider` capability; the standalone runner reads `PETRI_SECRET_<NAME>`; records are masked before they are appended | `crates/core/executor/src/secrets.rs` |
| Sandboxes | every provider (host, Docker, Daytona) through the sandbox-driver JSON-RPC plugin protocol; `Retention` (`Always` is the Fabro default), `petri sandbox prune` | `crates/core/executor-sandbox/`, `README.md` |
| Skills home, memory | the `FabroHome` capability (else `FABRO_HOME`, else `$HOME/.fabro`); project memory is read from the Git root to the working directory per Fabro's profile rules | `crates/fabro/steps/src/skills.rs`, `memory.rs` |
| Compaction policy, MCP tool registration, sub-agent limits | `CompactionPolicyHandle`; MCP servers from `[run.agent.mcps]` are Petri-owned processes and connections; Pebble's sub-agent tools are on every native agent | `crates/fabro/FORMAT.md` ("Native Pebble" and after) |

## Identities a host can rely on

- **Run and invocations.** The run declaration (`run.started`, the
  coordinator log's first record) names the run's key, its format version
  and the root invocation; the key is `RunOptions::run_key` when the host
  gave one, and the run id every sandbox of the run is labelled with. Every
  `RunEvent` carries `invocation` and `execution` when it has them, and
  `parent` (the calling execution, firing, attempt and call slot) on a nested
  invocation. A Fabro parallel branch is a child invocation; its entry node
  carries `meta.branch_role = {fork, index}`, the parent-side delegate
  `meta.branch = {fork, target, index}` and `synthetic: true`.
- **Stages.** `subject.node` is the node (`id`, instance `name`, step `kind`,
  the frontend's `meta` verbatim: `label`, `shape`, `kind` such as `command`,
  `agent`, `human`, `parallel`, `parallel.branch`, `parallel.fan_in`,
  `stack.manager_loop`, `classes`, `span`, `synthetic`). A host maps synthetic
  lowering nodes to the logical stage with `meta`, never with node names.
- **Firings, visits, attempts.** `firing` is the durable identity of one visit
  of a node in one execution; `visit` is its 1-based ordinal among the node's
  firings, `attempt` the 1-based retry within the firing, `generation` the
  loop generation. A retry keeps the firing and advances `attempt`; a loop
  starts a new firing and advances `visit`.
- **Branches.** `BranchRole` (`none`, `fork {branches}`, `member {fork,
  index}`, `join {fork}`) on every subject of a fan-out, static or
  `for_each`: the parallel node is the fork, each branch node or clone a
  member of its index, the fan-in the join. `fork.started`,
  `branch.completed` and `fork.completed` carry the same `BranchRef {fork,
  index}` for both and one `ForkOccurrence {execution, fork, firing, visit,
  generation}` per fork visit; the `fabro.parallel.*` payloads carry the
  index and the same occurrence (`{fork, firing}`), and a branch child's call
  slot is `branch:<fork>@<firing>:<index>:<target>`. A host keys every
  branch fact on the occurrence, never on the fork it saw last.
- **Interactions.** A question's identity is the node, the firing, its
  occurrence within the run, and the invocation path; `InterviewRequest` and
  the `step.progress.recorded` whose `parsed.question` is the question
  carry it, `control.requested` with a `derived.answer` closes it with an
  answer, a `parsed.expired` closes it when the gate's own deadline passes
  (with the default the gate took, when it had one), and the interview
  receipt (`<run_dir>/interviews.json`) keeps every question and its
  disposition (`answered`, `cancelled`, `failed`, `timed_out`).
- **Agent sessions.** Pebble's session id, parent session id, stream id and
  sequence, and tool call id are in every backend envelope a
  `step.progress.recorded` forwards as recorded, never rewritten. A retained thread keeps one session across the nodes
  that share it; `fabro.thread` names the thread and fidelity per node.
- **Model routes.** `fabro.fallback.route` carries the position in the plan,
  the provider and model, whether the session was reused, and the session id.
- **Sandboxes and workspaces.** `invocation.declared`'s `sandbox` is the binding;
  `petri inspect` reports every scope's workspace and the retrieval command
  for a container; the workspace survives success, failure and cancellation
  under `--retain always` (the Fabro default).

## Event positions

`EventId {log, seq, index}` is the position: the log the record came from
(`coordinator`, or `execution` with the id), the record's sequence in that
log, and the ordinal among the events one record produced (`0` is the
record's own event, which carries the stored line under `record`). Within one
log the order is total and causal (admission before start, start before
finish, the final finish before `visit.completed`, `visit.completed` before
`routing.resolved`, `routing.resolved` before `route.applied`, notes before
the record they annotate). Across executions `context.parent` and
`execution.declared`'s `predecessor` tie the streams together. A host records
the last `EventId` it has applied per log; on resume the driver
redelivers the regenerated suffix with the same identities, at least once,
and the host deduplicates by `EventId`. `replay_run` over the run directory
yields the same stream, event for event, floats included; `recorded_at` —
when each record was appended, read at the recording boundary and persisted
with it — is the same live and on replay, so run, stage, attempt and
interview times come from the logs, never from the time of a replay.
`observed_at` is the one live-only field. `run.paused` and `run.unpaused`
are coordinator records like any other, so replay carries them.

## Two shapes for the run record

**A. Mirror.** The run directory stays Petri's source of truth and Fabro
projects the public events into its own tables through `RunEventSink`.
Resume reads the run directory; the UI reads the database; a lossy sink is
completed with `replay_run` or `replay_since`, deduplicated by `EventId`.

**C. Records are the store.** Fabro implements `store::RunStore` and
`store::RunLogs` over its database and installs it with `Runtime::store`.
Petri's coordinator, every execution's engine log and the sandbox resource
log then live in Fabro's tables; resume, inspect and replay read them back
through the same handle; there is one source of truth. What Fabro's backend
must do:

- `open` takes the run's exclusive writer lease for the coordinator's
  `OwnerId`, idempotently for the same owner (a retry after a lost reply
  gets the same lease), and refuses another live owner with `Leased`. The
  lease ends when the handle is dropped, when Fabro's own liveness signal
  says the owner is gone (the server observes its worker's exit), or when an
  operator releases it; never by timeout. A handle whose lease moved gets
  `StaleOwner` on its next append.
- `append` returns once the records are durable, and `(log, seq)` is
  unique: the same record again is accepted without a second append, a
  different record at a taken seq is `Conflict`. A backend that keeps a
  derived view (`run_events`, `runs`) runs `execution::events::Projection`
  at ingest on its side of the seam and commits the record and its rows
  together, or the record first with the view's consumed positions beside
  its rows.
- `read` hands back every record of one log in seq order, unchanged as a
  JSON value; blobs come back byte-exact by digest.
- `testkit::run_store::conformance` is the contract; it runs against
  `RunDirStore` and `MemoryRunStore` in Petri and against Fabro's backend in
  Fabro.

What stays on the filesystem under either shape: sandbox workspaces, step
output under `logs/`, and artifacts. Large values already go through
`OutputStore`.

## Lifecycle acknowledgements

Every extension point is awaited at Petri's durability boundary, so a host's
acknowledgement gates the next step of the run:

- `before_attempt` runs before an attempt is dispatched; its decision and
  notes are recorded (`admission.decided`, a note in `step.progress.recorded`) before the attempt
  starts. Holding it pauses admission (the control service's pause is built
  on it).
- `prepare_result` runs after an attempt returned and before its record is
  appended, once per attempt. It is handed the effective outcome: the
  stage's failure policy, exhaustion included, has already run, so the
  final attempt (`will_retry == false`) is the completion to prepare, and
  routing follows the record it produces. An adjustment keeps the original
  evidence beside the effective record (a `result_prepared` note).
- `after_record` runs after the final outcome is recorded and before routing
  is resolved; its notes precede the routing record.
- `transition` runs after routes are selected and before they are recorded
  and applied; an override replaces the edge, a best-effort problem is
  recorded and the run continues, a `TransitionError` blocks every route
  (Fabro's fatal Git commit failure and best-effort metadata writes model
  onto these two).
- `run_finished` runs at the run's terminal exit before any environment is
  released; `scope_released` runs before each scope's own environment is
  released. Fabro's `run_complete`/`run_failed` (by final status, neither on a
  cancelled run) and `sandbox_cleanup` map onto them.
- `RunEventSink::deliver` is awaited per event behind a bounded queue
  (`ProjectorOptions::capacity`, 1024 by default); a slow sink delays
  delivery and never slows the run, an event that finds the queue full is
  counted as `overflowed` and left to the log, a failing sink stops the pump,
  and a `deliver` that outlasts `ProjectorOptions::stall_timeout` (30 seconds
  by default) is dropped and named in the receipt's `failure`, so shutdown is
  bounded. The `ProjectionReceipt` counts every undelivered event; recovery
  is `replay_run`, deduplicated by `EventId`.
- A step's progress is queued or acknowledged. `StepCtx::logs.send` orders
  the event ahead of the attempt's outcome (the driver's completion fence
  drains the queue before it records the outcome) without making it durable
  on its own; `send_acked` returns only once the record is appended and every
  observer's durable storage confirmed it (`EventObserver::durable`). The
  native agent backend records every Pebble event acknowledged, so Pebble's
  acknowledgement means the event is in Petri's log and a store that cannot
  write stops the prompt. The at-least-once limit is the attempt: one whose
  finish never landed is re-dispatched on resume and emits its events again.

## Compatibility versions

| Version | Where | Rule |
|---|---|---|
| `EVENT_CONTRACT_VERSION` (3) | `execution::events` | additive within a version; a host checks it before projecting. Version 3 names every event after its record, carries the stored line under `record` and the derived values under `derived`; the version 2 presentation names are gone |
| `INSPECT_FORMAT_VERSION` (3) | `execution::inspect` | the `petri inspect` document's field contract; version 3 reads the run through its store (`locator`, `run_key`; no log `path` or `torn`) |
| the run format (5) on the run declaration, and the coordinator record version (`{seq, origin, recorded_at, body}` lines, `body` tagged by `event` with `<subject>.<verb>` names; the declaration carries the run `key`) | `execution::store` | a run written by a newer or older format is refused, never migrated; the check reads the first stored record before any other is decoded |
| the engine log version (v10: `{seq, origin, recorded_at, body}` records, `body` tagged by `event` with `<subject>.<verb>` names), pinned by the run format | `engine::log` | a log whose version the runner does not speak is refused; replay must reproduce the log byte for byte or inspection reports corruption |
| `inspect_format_version`, `event_contract_version` | in the documents themselves | |
| Library pins (Pebble, lithos-llm, sandbox-driver, twins, the Fabro reference, the runner image) | `CONTRACT.md` "Pinned revisions", `scripts/check-pins.py` | moved together with the manifests and the evidence records |

**Strict rejection of incompatible logs.** Petri never migrates a stored
run: a run whose format version, coordinator record version, or engine log
version the runner does not speak is refused at resume and by `petri
inspect` (exit 2), and a log that does not replay to itself is reported as
corrupt. A run directory written before the run key existed (format 4 and
earlier) has no key in `run.json` and is refused as not a run. Fabro chooses which runner version serves which run and decides any
migration or old-runner retention policy; this work implements neither
automatic log migration nor a fleet of versioned runners.

## What stays Fabro's

Git-backed workspace restoration and checkpoints, run and meta branches,
pull requests and publication (including its deduplication), the database
and its transaction recovery, platform event migration, the UI and API,
notifications and Slack interviews, the vault, the MCP server catalog,
minted GitHub tokens, image builds from `image.dockerfile`, and the choice of
which runner version serves a run. Petri keeps local replay
(`petri inspect`, `replay_run`, resume from the run's store) and local
sandbox recovery (the plugin's lease, fence and reacquire on resume)
tested; those tests are listed in `CONTRACT.md` under "Local replay and
sandbox recovery".

## The integration checklist

In order. Each step has a Petri-side test a Fabro adapter can be checked
against.

1. **Implement the Petri interfaces in Fabro.** `ExecutionHooks` (the
   checkpoint, metadata and transition adapters), `HookService` if Fabro
   serves hooks itself (else keep `LocalHooks`), `Interviewer` over Fabro's
   questions API, `RunEventSink`, `SecretProvider` over the vault,
   `OutputStore` over platform storage, `CredentialProvider` for the model
   client, `FabroHome`, and, for shape C, `RunStore` and `RunLogs` over the
   database (in process first, or the worker's HTTP client), checked with
   `testkit::run_store::conformance`. The shape to copy is
   `crates/petri/lib/tests/embedding_readiness.rs`.
2. **Map stage and branch identities and the public events.** Project
   `RunEvent`s onto Fabro's event families with the matrix in `EVENTS.md`;
   key stages on `subject.node.meta`, branches on `BranchRole` and
   `meta.branch_role`, agent facts on the Pebble envelope's identities.
   Validate the adapter against the projection tests named in the matrix;
   Fabro's platform event schema stays in Fabro.
3. **Connect platform hooks and storage.** Fabro's `run_start`,
   `sandbox_ready`, `stage_*`, `edge_selected`, `parallel_*`, `run_*` and
   `sandbox_cleanup` hooks already run through `LocalHooks`; platform-side
   effects (checkpoint commits, database writes) go into `transition` and
   `prepare_result`, with a fatal failure as `TransitionError` and a
   best-effort one as a recorded problem. Large values and artifacts go
   through `OutputStore`. Under shape C, Petri's records and Fabro's derived
   rows commit in one transaction, or the record first.
4. **Restore workspaces and interactions before dispatching resumed work.**
   Petri resumes from the run's store (`Runtime::store` with the run's key
   in `RunOptions::run_key`, or the run directory) and reacquires held
   sandboxes, reconciling every lease with the provider by label before any
   create; Fabro restores what it owns (a Git-backed workspace, pending
   questions in its UI) from the identities above, then resumes. Known limits: a retained
   thread is not durable across resume (the node starts a fresh session with
   Fabro's discarded-session rule), a pause does not survive resume (a
   resumed run starts unpaused), a model request in flight at the crash may
   be sent again, and an external effect is at least once.
5. **Choose compatible runners.** Pin the Petri runner per run; check
   `EVENT_CONTRACT_VERSION`, `INSPECT_FORMAT_VERSION` and the store versions
   before resuming; keep an old runner for old runs as long as Fabro's
   retention policy requires.
6. **Roll out new runs.** Start new runs on the Petri runner with the
   readiness suites as the acceptance gate (`mise run test:fabro:blackbox`,
   `mise run test:fabro:differential`), then widen. The ACP backend is
   covered by the `acp` scenario family through a scripted agent on the host
   and in a container; real ACP client products (Claude Code, Gemini CLI),
   Daytona and crash-resume across runner versions have their own gates and
   are not part of the initial readiness claim.
