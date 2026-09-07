# The public event contract

`execution::events` is the versioned event stream an embedding host projects a
run from. This file is the contract for `EVENT_CONTRACT_VERSION` 1. The Rust
types in `crates/core/execution/src/events.rs` are authoritative for field
detail; this file states the guarantees.

## Sources and identity

Every `RunEvent` is derived from one durable record: a coordinator record
(`coordinator.jsonl`) or an engine record (one execution's `events.jsonl`) with
the post-apply engine state beside it. The derivation is the same live (the
`EventProjector` observer) and over a finished run dir (`replay_run`).

`EventId { source, seq, index }` is the stable identity: the log the record came
from (`coordinator`, or `execution: <id>`), the record's `seq` in that log, and
the ordinal of this event among the events one record produced. Within one
source the order is total. Every event carries `invocation` and `execution`
when it has them, and `parent` (the calling execution, firing, attempt and call
slot) for a nested invocation.

`subject` names the node (`NodeRef`: id, instance name, step kind, the
frontend's `meta` verbatim) and, when the event is about a firing, the firing
id, the visit ordinal (which firing of the node within the execution, 1-based),
the attempt (1-based within the firing), the generation, and the node's
`BranchRole` (`none`, `fork`, `member` of a fork's branch, or `join`).

Visits and attempts are distinct: a retry advances `attempt` and keeps the
firing and visit; a loop that fires the node again starts a new firing and
advances `visit`.

## Source metadata

`subject.node.meta` is the frontend's node metadata. The Fabro frontend sets
`label`, `shape`, `kind` (`start`, `exit`, `command`, `agent`, `human`,
`parallel`, `parallel.branch`, `parallel.fan_in`, `wait`, `stack.manager_loop`,
`goal_check`, ...), `classes`, `span`, and `synthetic: true` on nodes it
invented. A `parallel.branch` node is the parent-side delegate of one branch;
its `meta.branch = { fork, target, index }` names the branch, and the branch's
stage itself runs in the child invocation the delegate starts (the child's
entry node carries `meta.branch_role = { fork, index }`). A synthetic
`<fork>.fan_in` is a `parallel.fan_in` with `synthetic: true`. A host
distinguishes logical stages from lowering artifacts with these fields and
with `BranchRole`, never with node names.

## Events

Run and invocation events (coordinator log): `run_started`, `run_finished`,
`invocation_declared` (with the parent link, graph digest, sandbox binding and
initial context), `invocation_finished` (with the `InvocationResult`),
`invocation_cancel_requested` (with the `reason` the requester gave, when it
gave one: `stall_timeout`, `interrupt`, `control`), `stall_timeout` (derived
beside the cancel request the stall watchdog made, with the budget and the
idle time), `execution_declared` (predecessor, index, entry),
`execution_finished` (the engine exit: terminal status or restart).

Live-only host notices (`EventSource::Host`, `seq` counting the projector's
notices): `run_paused`, `run_unpaused`, published by a projector that follows
the control service (`EventProjector::follow_controls`). Nothing durable backs
them, so replay does not carry them.

Execution events (engine log), each attributed to a subject where one exists:

| Event | When |
| --- | --- |
| `execution_started`, `execution_admitted` | the execution's own start and admission |
| `visit_started` | a firing exists for a node whose join was satisfied |
| `attempt_admitted` | the host or middleware decided on an attempt (`Admit`, `Skip`, `Block`, with the trace) |
| `attempt_started` | the attempt was dispatched to its step |
| `attempt_finished` | an attempt returned; `final` says whether it is the firing's outcome, `exhausted` whether retries ran out |
| `retry_scheduled`, `retry_elapsed` | the backoff between attempts |
| `visit_completed` | the firing's final record exists; `executed` is false for a synthesized completion (false precondition, cancelled scope, blocked or skipped admission); `attempts` is the count |
| `routes_resolved` | one `RouteChoice` per group with the decision, resolved target, interventions (overrides, jumps, blocks) and whether a weighted draw happened |
| `route_applied` | one applied route: edge (with target, transition, back), jump, or none |
| `fork_started`, `branch_completed`, `fork_completed` | a fork's branches start; each branch's last token reaches the join; the join fires with every branch result in branch order |
| `node_expanded` | a `for_each` expansion with its clones |
| `question_asked`, `control_delivered` | a question on the firing's progress channel; a host control decoded as an answer when it is one, with whether the firing could receive it |
| `wait_state_changed` | `awaiting_admission`, `running`, `awaiting_answer`, `awaiting_retry`, `cancelling` |
| `cancel_requested`, `kill_requested` | the two stop tiers, scope or group |
| `output_line`, `artifact_recorded` | step output and artifacts |
| `agent_activity` | a backend's own event envelope (`kind` names the backend; for `pebble` the envelope is Pebble's `CodingAgentEvent`) with the session, parent session, tool call, stream and stream sequence read out of it. A native agent's sub-agents are on the same stream: the lifecycle (`SubAgentSpawned`, `SubAgentTurnStarted`, `SubAgentCompleted`, `SubAgentFailed`, `SubAgentClosed`) under the parent's session, a child's own events under the child's session with `parent_session` naming its immediate parent, all attributed to the parent stage (`crates/fabro/FORMAT.md`, "Native Pebble") |
| `budget_paused`, `budget_resumed` | an executor-enforced attempt budget stopped counting (the attempt asked a question; `remaining_ms` is the active-work time left, `pending_questions` how many wait) and counted again (its last pending question was answered); from the driver's durable `budget_paused`/`budget_resumed` notes |
| `host_note` | a `driver::lifecycle::Note` the host or the driver recorded: `result_prepared` (original attempt evidence beside an adjusted result), `transition` (overrides, best-effort problems, a block), `hook` (a hook service report) |
| `step_custom` | any other step-defined progress payload |

Usage and timing: `attempt_finished` and `visit_completed` carry the outcome's
`metrics` (`duration_ms`, `exit_code`, `custom`). The driver fills
`duration_ms` with the observed wall-clock duration of the attempt when the step
kind did not report one. The native agent backend reports `pebble.usage`,
`pebble.cost_usd_micros`, `pebble.inference_ms`, `pebble.tool_ms` and
`pebble.subagents` (the node's agent tree: children spawned, completed,
failed and closed, and their summed usage by session) under `custom`.

## Ordering and delivery

- Per execution, events are delivered in record order, and within one record
  in `index` order. The order is causal: admission before start, start before
  finish, the final finish before `visit_completed`, `visit_completed` before
  `routes_resolved`, `routes_resolved` before `route_applied`, host notes
  before the record they annotate.
- Across executions the `parent` link and `execution_declared.predecessor` tie
  the streams together. The coordinator log's records are delivered as they
  are appended; a fresh run's `run_started` is delivered to an observer when
  it attaches.
- `EventProjector` is lossless: the observer callback derives and queues, a
  pump task awaits the host's `RunEventSink::deliver` per event, so a slow
  sink delays and never drops. A sink error stops the pump; later events are
  counted as undelivered and the `ProjectionReceipt` says so. Recovery is
  `replay_run`.
- Across a resume, the driver delivers the regenerated suffix (the records a
  crash kept off disk) before dispatching pending work, with the same
  identities; delivery is at-least-once, deduplicated by `EventId`. A
  projector attached at resume is built with `EventProjector::primed`, which
  folds the on-disk prefix into its state without delivering it.
- Every event is derived from a durable record, output lines included, except
  the `Host`-sourced notices (`run_paused`, `run_unpaused`), which are
  live-only by design. The one live-only field on a derived event is
  `observed_at` (milliseconds since the epoch when the projector saw the
  record), absent on replay. A backend's live stream chunks that never reached
  the step's progress channel are not in the contract.

## Secrets

Records are masked by the driver before they are appended, so every value is
post-mask: a secret reference stays `{"$secret": ...}`, a masked value stays
`***`. Host notes are masked the same way before they are recorded.

## Terminal output

The CLI's terminal rendering (`[node#firing] line`, status lines) is a
presentation over these events and the run dir. It is not part of the
contract; a host consumes `RunEvent`s and never parses terminal text.

## Known backend limits

- The native agent backend (`pebble`) records every `CodingAgentEvent` as a
  `StepEvent::Custom`; it is in the log and therefore durable.
- The ACP backend records what the external agent sends over ACP; tool calls
  the agent does not report are not observable.
- Agent facts are Pebble's; Petri adds run, invocation, node and attempt
  attribution and does not restate them.
