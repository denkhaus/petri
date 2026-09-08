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

Run controls (coordinator log): `run_paused`, `run_unpaused`, derived from
the `RunPaused` and `RunUnpaused` records the control service appends through
the coordinator. Replay carries them, and a resume whose last recorded control
is a pause starts with admission held. The records are additive to coordinator
format version 2.

Run-level notes (coordinator log): a `host_note` with no subject, derived from
a `RunNote` record: the report of a hook point that belongs to no firing
(`run_finished`, `scope_released`), with the execution whose driver ran it.
The coordinator appends them from the execution's report, in the order the
points ran, before it records `RunFinished`. Additive to coordinator format
version 2.

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
| `fork_started`, `branch_completed`, `fork_completed` | a fork's branches start; each branch's last token reaches the join; the join fires with every branch result in branch order. A static fork's branches are its routing groups; a `for_each` expansion's branches are its clones, in item order, and the fork is the node that fanned out into the template (Fabro's `parallel` node), so both fan-outs carry the same identities |
| `node_expanded` | a `for_each` expansion with its clones |
| `question_asked`, `control_delivered` | a question on the firing's progress channel; a host control decoded as an answer when it is one, with whether the firing could receive it |
| `wait_state_changed` | `awaiting_admission`, `running`, `awaiting_answer`, `awaiting_retry`, `cancelling` |
| `cancel_requested`, `kill_requested` | the two stop tiers, scope or group |
| `output_line`, `artifact_recorded` | step output and artifacts |
| `agent_activity` | a backend's own event envelope (`kind` names the backend; for `pebble` the envelope is Pebble's `CodingAgentEvent`) with the session, parent session, tool call, stream and stream sequence read out of it. A native agent's sub-agents are on the same stream: the lifecycle (`SubAgentSpawned`, `SubAgentTurnStarted`, `SubAgentCompleted`, `SubAgentFailed`, `SubAgentClosed`) under the parent's session, a child's own events under the child's session with `parent_session` naming its immediate parent, all attributed to the parent stage (`crates/fabro/FORMAT.md`, "Native Pebble") |
| `budget_paused`, `budget_resumed` | an executor-enforced attempt budget stopped counting (the attempt asked a question; `remaining_ms` is the active-work time left, `pending_questions` how many wait) and counted again (its last pending question was answered); from the driver's durable `budget_paused`/`budget_resumed` notes |
| `host_note` | a `driver::lifecycle::Note` the host or the driver recorded: `result_prepared` (original attempt evidence beside an adjusted result), `transition` (overrides, best-effort problems, a block), `hook` (a hook service report). A run-level hook report is the same `hook` note from the coordinator log, with no subject |
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
- Every event is derived from a durable record, output lines included. The
  one live-only field on a derived event is `observed_at` (milliseconds since
  the epoch when the projector saw the record), absent on replay. A backend's
  live stream chunks that never reached the step's progress channel are not
  in the contract.

## Secrets

Records are masked by the driver before they are appended, so every value is
post-mask: a secret reference stays `{"$secret": ...}`, a masked value stays
`***`. Host notes are masked the same way before they are recorded.

## Terminal output

The CLI's terminal rendering (`[node#firing] line`, status lines) is a
presentation over these events and the run dir. It is not part of the
contract; a host consumes `RunEvent`s and never parses terminal text.

## Event-coverage matrix

The final event contract for readiness item 7, completed at milestone D
(readiness item 10): every Fabro execution-related need, the public Petri
source that carries it, the identities a host keys on, whether the fact is a
durable record (in the log, delivered live and on replay) or live-only, and
the projection test that proves it from `RunEvent`s alone. The Fabro names
describe the consumer's need, not Petri event names. A `step_custom` row
names the `kind` of the `StepEvent::Custom` payload; every such payload
also carries `node`, `firing` and `attempt` beside the event's `subject`.

| Fabro need | Public source | Identities | Durability | Projection test |
| --- | --- | --- | --- | --- |
| `run.started/completed/failed`, root vs internal invocation | `run_started {root, middleware_chain}`, `run_finished {status}`, `invocation_declared` (`call` is `None` on the root, a `ParentLink` on a child), `invocation_finished {result}`, `execution_declared/finished` | `EventId`, `invocation`, `execution`, `parent` | durable (coordinator log) | `embedding::the_workflow_runs_without_adapters_and_the_events_reconstruct_it`, `embedding_readiness` |
| run notices, steer, interrupt, cancel reasons | `invocation_cancel_requested {reason}` (`stall_timeout`, `interrupt`, `control`), `cancel_requested`, `kill_requested`, `control_delivered {Deliver}` (a steer is `{"$steer": ...}`) | invocation, firing | durable | `fabro_readiness_blackbox::…_is_cancelled_…` (`reason = interrupt`), `controls::an_idle_run_is_cancelled_by_the_watchdog` (`stall_timeout`), `controls::the_control_file_pauses_unpauses_and_steers_without_answering` |
| `stage.started/completed/failed/retrying` | `visit_started`, `attempt_admitted`, `attempt_started`, `attempt_finished {final, exhausted}`, `retry_scheduled`, `retry_elapsed`, `visit_completed {executed, attempts}`; `subject.node.meta.kind` and `synthetic` map lowering nodes to the logical stage | node, firing, visit, attempt, generation | durable | `embedding::the_workflow_runs_without_adapters_…` (the retry), `embedding_readiness` (every logical stage's final status) |
| `stage.prompt`, `prompt.completed` | `step_custom` kinds `fabro.prompt`, `fabro.prompt.completed`; a prompt node's `attempt_finished` output | node, firing, attempt | durable | `petri-fabro-steps::prompt` (prompt events), `fabro_blackbox::a_prompt_node_makes_one_tool_free_model_call` |
| `edge.selected`, `loop.restart` | `routes_resolved {choices}` (decision, target, overrides, jumps, blocks, weighted draw), `route_applied {edge / jump / none, transition, back}`; a restart is `execution_finished {Restart}` then `execution_declared {predecessor}` | firing, edge, execution | durable | `embedding::transitions_override_block_or_continue`, `controls::node_visit_totals_survive_a_restart_while_context_resets` |
| `parallel.started`, branch start and completion, `parallel.completed` (static fan-out) | `fork_started {branches}`, `branch_completed {result}`, `fork_completed {fork, results}` in branch order; `BranchRole` on every subject | fork node, `BranchRef {fork, index}` | durable | `embedding::the_workflow_runs_without_adapters_…` (`forks`, `joins`) |
| the same for a `for_each` fan-out (an expansion) | the same three bodies, with the same identities: `fork_started {branches}` on the parallel node once the expansion knows its items (one `BranchRef {fork, index}` per item, in item order), `branch_completed {result}` per clone as its token reaches the fan-in, `fork_completed {fork, results}` at the fan-in in item order; the clones are `member {fork, index}`, the fan-in `join {fork}`. Beside them: `node_expanded {clones}`; one `invocation_declared` per branch child with its `parent` link, `invocation_finished` per child; `step_custom` kinds `fabro.parallel.branch.started`, `fabro.parallel.branch.completed` (the delegates) and `fabro.parallel.completed` (the fan-in, with `parallel.results`). The roles come from one mechanism for both fan-outs: `BranchMap::of` reads the graph shape, `BranchMap::with_expansions` reads the engine's applied splices (the template, its clones by item index, the one node whose forward arm reaches the template as the fork) | fork node, `BranchRef {fork, index}`, child invocation | durable | `embedding::the_milestone_workflow_runs_through_the_embedding_boundary` (`fork:2`, `join`, `forks`, `joins`; live equals replay), `fabro_blackbox::for_each_branches_keep_distinct_values_under_one_key_in_item_order` (the three bodies through `replay_run`), `embedding_readiness`, `fabro_readiness_blackbox` (one expansion, one fork, one join, two children) |
| `interview.started/completed/timeout/interrupted` | `question_asked {question}` (type, choices, interaction identity), `wait_state_changed {awaiting_answer}`, `control_delivered {Answer, deliverable}`; expiry and interruption are the attempt's `TimedOut`/`Cancelled` status; a sensitive answer stays `{"$secret": …}` | node, firing, attempt, question id | durable | `embedding::the_workflow_runs_without_adapters_…`, `embedding_readiness` (`questions`, `answers`), `petri::interview` (the interviewer contract), `inspect_cli::inspect_shows_a_sensitive_answer_as_a_secret_reference_only` |
| `command.started/completed` | `attempt_started`, `attempt_finished {outcome}` (`metrics.exit_code`, `duration_ms`, `output`), `output_line`, `artifact_recorded` | node, firing, attempt | durable | `embedding::the_workflow_runs_without_adapters_…` |
| `agent.*` session, tool calls, LLM requests, steering | `agent_activity {backend, session, parent_session, tool_call, stream, stream_seq, envelope}`: Pebble's own `CodingAgentEvent` envelope, one stream per session | session id, stream id and sequence, tool call id | durable | `fabro_subagents_blackbox::a_parent_delegates_a_workspace_change_to_a_child`, `embedding_readiness` |
| `agent.*` threads (retained sessions, fidelity) | `step_custom` kind `fabro.thread` (thread id, fidelity, resolution) once per native session | node, firing, attempt, session | durable | `fabro_readiness_blackbox`, `fabro_hooks_blackbox::full_fidelity_nodes_share_one_conversation_through_the_binary` |
| `agent.failover` (C1): plan, per-target requests, decisions, accounting | `step_custom` kinds `fabro.fallback.plan` (routes, notices), `fabro.fallback.route` (position, route, `reused`, session), `fabro.fallback.usage` (per position: outcome, usage, cost, timings), `fabro.fallback.failover` (from, to, typed error, `continuation`), `fabro.fallback.stop` (`ineligible`/`exhausted`, last error); `attempt_finished.metrics.custom.fallback.*` | node, firing, attempt, position, session | durable (a resumed node starts a new plan) | `fallback_events` (3), `fabro_readiness_blackbox` (failover, then `stop = exhausted` in the failure case), `embedding_readiness` |
| `agent.mcp.*` (C2): server and tool lifecycle | `step_custom` kinds `fabro.mcp.server` (`starting`, `ready` with the tools, `failed`, `disconnected`, `stopped`) and `fabro.mcp.tool` (server, tool, tool call id, status, duration); the call itself is Pebble's `ToolCallStarted/Completed` in `agent_activity` under `mcp__<server>__<tool>` | node, firing, attempt, server name, tool call id | durable | `fabro_readiness_blackbox`, `embedding_readiness` (through `replay_run`); raw-log reads in `fabro_mcp_blackbox` |
| `agent.skills.*` (C3): discovery and loading | `step_custom` kinds `fabro.skills` (the ordered directories with their sources, once per native session) and `fabro.skills.warning` (`malformed`, `unreadable`, `missing_directory`); Pebble's `SkillsDiscovered`/`SkillActivated` in `agent_activity` | node, firing, attempt, scope | durable | `fabro_readiness_blackbox`, `embedding_readiness`; raw-log reads in `fabro_skills_blackbox` |
| `agent.subagent.*` (C4): spawn, input, wait, close, child usage | `agent_activity` under the parent's session: `SubAgentSpawned {agent_id, depth, task}`, `SubAgentCompleted`, `SubAgentFailed`, `SubAgentClosed`; the child's own events under the child's session with `parent_session`; `attempt_finished.metrics.custom.pebble.subagents` (counts and per-session usage) | parent session, child session, stream sequence | durable | `fabro_subagents_blackbox::a_parent_delegates_a_workspace_change_to_a_child` (usage rebuilt from events equals the metric), `fabro_readiness_blackbox`, `embedding_readiness` |
| `agent.compaction.*` (C5): lifecycle and summary usage | Pebble's `CompactionStarted/Completed/Failed/Cancelled` in `agent_activity`; `step_custom` kind `fabro.compaction` (the summary call's usage and cost) once per compaction; `attempt_finished.metrics.custom.pebble.compaction_*` | node, firing, attempt, session | durable | `petri-fabro-steps::compaction::public_events_account_for_the_compaction_and_later_activity`, `fabro_readiness_blackbox`, `embedding_readiness` |
| `agent.*` error and warning | `attempt_finished` failure class; `step_custom` kinds `fabro.hook.warning` (an unenforceable ACP hook), `fabro.skills.warning`, `fabro.mcp.server {failed}` | node, firing, attempt | durable | `petri-fabro-steps::hooks::acp_tool_hooks_are_best_effort_with_explicit_warnings`, `fabro_mcp_blackbox` |
| `watchdog.timeout` | `stall_timeout {stall_timeout_ms, idle_ms}` beside `invocation_cancel_requested {reason: stall_timeout}` | invocation | durable (coordinator log) | `controls::an_idle_run_is_cancelled_by_the_watchdog` |
| `subgraph.started/completed` | `invocation_declared {call: ParentLink}`, `invocation_finished`, `execution_*` with `parent` | invocation, parent execution, firing, attempt, call slot | durable | `petri-execution::inspect::a_nested_invocation_keeps_its_own_context_and_parent_link`, `petri-fabro-acceptance::workflow` |
| local setup (`[run.prepare]`, `[run.clone]`) | the `run_prepare_N` stages' events; `step_custom` kind `fabro.checkout` (repository, commit, depth, files) on `start` | node, firing | durable | `embedding::the_milestone_workflow_runs_through_the_embedding_boundary`, `fabro_scenarios_blackbox` (checkout) |
| hook decisions (workflow points) | `host_note {kind: "hook"}` with the `HookReport` (point, decision, each hook's state and duration, fail-open warnings); a report that ran no hook is silent | node, firing, attempt | durable | `embedding::a_hook_service_runs_each_hook_once_at_its_point`, `petri-fabro-steps::hooks` |
| hook decisions (tool boundary) | `step_custom` kind `fabro.hook` (`event` = `pre_tool_use`, `post_tool_use`, `post_tool_use_failure`, and the report), one per tool hook that ran, a child's under the parent stage | node, firing, attempt | durable | `fabro_readiness_blackbox`, `embedding_readiness` (counts per stage, two `block` decisions) |
| run-level hooks (`run_complete`, `run_failed`, `sandbox_cleanup`) | `host_note {kind: "hook"}` from the coordinator log's `RunNote` records, with no subject and the execution named: one per run-level point that ran a hook (`point` is `run_finished` or `scope_released`), in the order the points ran, before `run_finished`; the run's own end is `run_finished` | execution | durable (coordinator log) | `fabro_milestone_blackbox` (`assert_run_level_notes`: the reports through `replay_run` and `petri inspect` on a succeeded, a failed and a cancelled run, beside the hooks' effects) |
| local sandbox, retention, output references | `invocation_declared.sandbox` (the binding), the reported workspace in `petri inspect`, `output_line`, `artifact_recorded`, `blob://sha256/…` references in outputs; the acquisition progress lines are terminal-only | invocation, scope | durable (the binding and outputs), live-only (progress lines) | `inspect_cli`, `petri-fabro-steps::steps::large_command_output_is_offloaded_and_reads_back_logically` |
| budget pause and resume | `budget_paused {remaining_ms, pending_questions}`, `budget_resumed {remaining_ms}` from the driver's notes | node, firing, attempt | durable | `petri-driver::interview_budget::the_waiting_stage_pays_only_for_active_work` |
| pause and unpause | `run_paused`, `run_unpaused` from the coordinator's `RunPaused` and `RunUnpaused` records | run | durable; a resume starts paused when the last control recorded is a pause | `controls::pause_holds_admission_and_unpause_releases_it`, `controls::a_pause_survives_resume_and_holds_admission_until_unpaused`, `fabro_resume_blackbox::a_paused_run_stays_paused_across_resume_until_unpaused` |
| platform lifecycle, `checkpoint.*`, `git.*`, `pull_request.*`, product projections | not emitted; a host performs them in its `transition` and records `host_note {kind: "transition"}` | | | `embedding::adapters_run_in_order_and_checkpoint_work_follows_source_metadata` |

Replay equality: `embedding_readiness::the_combined_workflow_runs_through_the_embedding_boundary`
compares the live stream with `replay_run` event for event over a run that
exercises every row above but the live-only ones (624 events). Floats inside
backend payloads survive the round trip exactly because the workspace's
`serde_json` enables `float_roundtrip`.

## Known backend limits

- The native agent backend (`pebble`) records every `CodingAgentEvent` as a
  `StepEvent::Custom`; it is in the log and therefore durable. Compaction of
  the agent's context appears in that stream as Pebble's `CompactionStarted`,
  `CompactionCompleted`, `CompactionFailed` and `CompactionCancelled`. Those
  events carry no usage, so the Fabro backend adds one `step_custom` per
  compaction with `kind = "fabro.compaction"` carrying the summary call's
  usage, which Pebble bills to the prompt that compacted; see
  `crates/fabro/FORMAT.md`, "Compaction".
- The ACP backend records what the external agent sends over ACP; tool calls
  the agent does not report are not observable.
- Agent facts are Pebble's; Petri adds run, invocation, node and attempt
  attribution and does not restate them.
- A backend envelope is a `step_custom` object with a string `kind` and an
  `event` **object**. A step's own payload may carry a string `event` (a
  hook report names its hook event); it stays a `step_custom`.
- A `for_each` child invocation's own stage carries no `branch_role` in its
  `meta`: the item index is known only at run time. The parent-side clone
  (`<template>#<index>`, a `parallel.branch` delegate) carries the member
  role, and the child's `meta.branch = {fork, target}` names the fork.
