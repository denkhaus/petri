# The Fabro integration handoff

This page is what a host that embeds Petri to run Fabro workflows can rely
on, and the order of the work that remains on the Fabro side. Readiness item
10 (`.ai/plans/fabro-execution-readiness.md`) asked for it; the evidence
behind every claim is `crates/fabro/acceptance/CONTRACT.md` (the support
matrix, the readiness audit) and `crates/core/execution/EVENTS.md` (the event
contract and its coverage matrix).

Petri owns workflow semantics, execution, the durable run directory, the
public event stream and the extension points. Pebble owns the agent loop.
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
| Load and run one workflow | `Runtime::check` (lowering with diagnostics), `execution::host::HostRun`, `host::run_configured` | `crates/core/execution/src/host.rs` |
| Awaited extension points: admission, result preparation, transition, run end, scope release | `driver::lifecycle::ExecutionHooks`, installed with `Runtime::hooks`; `AdmitAttempt` (`Admit`, `Skip`, `Block`), `PrepareResult` (`Prepared` adjustments with the original evidence kept), `Transition` (`RouteOverride`, best-effort `problems`, a fatal `TransitionError`), `RunFinished`, `ScopeReleased`; notes returned at each point are durable records | `crates/core/driver/src/lifecycle.rs`; proven by `crates/petri/lib/tests/embedding.rs` and `embedding_readiness.rs` |
| The local hook system, or a replacement | `execution::hooks::HookService` behind `HookAdapter` and the `HookServiceHandle` capability; the standalone service is `fabro_steps::hooks::LocalHooks`. A host that installs its own `ExecutionHooks` and still wants `[[run.hooks]]` wraps `HookAdapter` and forwards every point, `run_finished` and `scope_released` included (the `EmbeddingHost` in `embedding_readiness.rs` is the pattern) | `crates/core/execution/HOOKS.md` |
| Questions and answers | `execution::interview::{Interviewer, InterviewDispatcher}`; `InterviewRequest` carries the interaction identity (node, firing, occurrence, invocation path), the question type and choices; `InterviewReply::Answered(Answer)`, expiry, cancellation | `crates/core/execution/src/interview.rs` |
| Pause, unpause, steer, cancel | `execution::controls` (the control service; `petri run --control <FILE>` is the terminal transport), `RunHandle` for cancel and kill | `crates/core/execution/src/controls.rs` |
| The public event stream | `execution::events::{EventProjector, RunEventSink, replay_run}`; `EVENT_CONTRACT_VERSION` | `crates/core/execution/EVENTS.md` |
| Durable inspection of a run directory | `execution::inspect::inspect_run` (`petri inspect --run-dir --json`), `INSPECT_FORMAT_VERSION` | `crates/core/execution/INSPECT.md` |
| Output references and large values | the `OutputStore` capability (`BlobStore`); the default is a local store under `<run_dir>/blobs` writing `blob://sha256/<hex>` | `crates/fabro/steps/src/blobs.rs` |
| Secrets | the `SecretProvider` capability; the standalone runner reads `PETRI_SECRET_<NAME>`; records are masked before they are appended | `crates/core/executor/src/secrets.rs` |
| Sandboxes | every provider (host, Docker, Daytona) through the sandbox-driver JSON-RPC plugin protocol; `Retention` (`Always` is the Fabro default), `petri sandbox prune` | `crates/core/executor-sandbox/`, `README.md` |
| Skills home, memory | the `FabroHome` capability (else `FABRO_HOME`, else `$HOME/.fabro`); project memory is read from the Git root to the working directory per Fabro's profile rules | `crates/fabro/steps/src/skills.rs`, `memory.rs` |
| Compaction policy, MCP tool registration, sub-agent limits | `CompactionPolicyHandle`; MCP servers from `[run.agent.mcps]` are Petri-owned processes and connections; Pebble's sub-agent tools are on every native agent | `crates/fabro/FORMAT.md` ("Native Pebble" and after) |

## Identities a host can rely on

- **Run and invocations.** `run.json` names the root invocation. Every
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
  member of its index, the fan-in the join. `fork_started`,
  `branch_completed` and `fork_completed` carry the same `BranchRef {fork,
  index}` for both; the `fabro.parallel.*` payloads carry the index as well.
- **Interactions.** A question's identity is the node, the firing, its
  occurrence within the run, and the invocation path; `InterviewRequest` and
  `question_asked` carry it, `control_delivered {Answer}` closes it, and the
  interview receipt (`<run_dir>/interviews.json`) keeps every question and
  reply.
- **Agent sessions.** Pebble's session id, parent session id, stream id and
  sequence, and tool call id are read out of every `agent_activity` envelope
  and never rewritten. A retained thread keeps one session across the nodes
  that share it; `fabro.thread` names the thread and fidelity per node.
- **Model routes.** `fabro.fallback.route` carries the position in the plan,
  the provider and model, whether the session was reused, and the session id.
- **Sandboxes and workspaces.** `invocation_declared.sandbox` is the binding;
  `petri inspect` reports every scope's workspace and the retrieval command
  for a container; the workspace survives success, failure and cancellation
  under `--retain always` (the Fabro default).

## Event positions

`EventId {source, seq, index}` is the position: the log the record came from
(`coordinator`, or `execution: <id>`), the record's sequence in that log, and
the ordinal among the events one record produced. Within one source the order
is total and causal (admission before start, start before finish, the final
finish before `visit_completed`, `visit_completed` before `routes_resolved`,
`routes_resolved` before `route_applied`, notes before the record they
annotate). Across executions the `parent` link and
`execution_declared.predecessor` tie the streams together. A host records
the last `EventId` it has applied per source; on resume the driver
redelivers the regenerated suffix with the same identities, at least once,
and the host deduplicates by `EventId`. `replay_run` over the run directory
yields the same stream, event for event, floats included; `observed_at` is
the one live-only field. `run_paused` and `run_unpaused` are the two
live-only notices.

## Lifecycle acknowledgements

Every extension point is awaited at Petri's durability boundary, so a host's
acknowledgement gates the next step of the run:

- `before_attempt` runs before an attempt is dispatched; its decision and
  notes are recorded (`attempt_admitted`, `host_note`) before the attempt
  starts. Holding it pauses admission (the control service's pause is built
  on it).
- `prepare_result` runs after an attempt returned and before its record is
  appended; an adjustment keeps the original evidence beside the effective
  record (`host_note {kind: result_prepared}`).
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
- `RunEventSink::deliver` is awaited per event; a slow sink delays and never
  drops, a failing sink stops the pump and the `ProjectionReceipt` counts the
  undelivered events; recovery is `replay_run`.

## Compatibility versions

| Version | Where | Rule |
|---|---|---|
| `EVENT_CONTRACT_VERSION` (1) | `execution::events` | additive within a version; a host checks it before projecting |
| `INSPECT_FORMAT_VERSION` (1) | `execution::inspect` | the `petri inspect` document's field contract |
| the run-directory format (`run.json`) and the coordinator record version | `execution::store` | a run written by a newer or older format is refused, never migrated |
| the engine log version (`Log` records, v8) | `engine::log` | a log whose version the runner does not speak is refused; replay must reproduce the log byte for byte or inspection reports corruption |
| `inspect_format_version`, `event_contract_version` | in the documents themselves | |
| Library pins (Pebble, lithos-llm, sandbox-driver, twins, the Fabro reference, the runner image) | `CONTRACT.md` "Pinned revisions", `scripts/check-pins.py` | moved together with the manifests and the evidence records |

**Strict rejection of incompatible logs.** Petri never migrates a run
directory: a run whose format version, coordinator record version, or engine
log version the runner does not speak is refused at resume and by `petri
inspect` (exit 2), and a log that does not replay to itself is reported as
corrupt. Fabro chooses which runner version serves which run and decides any
migration or old-runner retention policy; this work implements neither
automatic log migration nor a fleet of versioned runners.

## What stays Fabro's

Git-backed workspace restoration and checkpoints, run and meta branches,
pull requests and publication (including its deduplication), the database
and its transaction recovery, platform event migration, the UI and API,
notifications and Slack interviews, the vault, the MCP server catalog,
minted GitHub tokens, image builds from `image.dockerfile`, and the choice of
which runner version serves a run. Petri keeps local replay
(`petri inspect`, `replay_run`, resume from the run directory) and local
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
   client, `FabroHome`. The shape to copy is `crates/petri/lib/tests/embedding_readiness.rs`.
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
   through `OutputStore`.
4. **Restore workspaces and interactions before dispatching resumed work.**
   Petri resumes from the run directory and reacquires held sandboxes; Fabro
   restores what it owns (a Git-backed workspace, pending questions in its
   UI) from the identities above, then resumes. Known limits: a retained
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
   `mise run test:fabro:differential`), then widen. ACP clients, Daytona and
   crash-resume across runner versions have their own gates and are not part
   of the initial readiness claim.
