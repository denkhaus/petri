# The hook service boundary

`execution::hooks` is the one interface a hook executor implements, and the
one adapter that calls it from the driver's awaited extension points. This
file is for the local implementation (readiness item 5, task 8) and for an
embedding host that replaces it.

## The interface

```rust
#[async_trait]
pub trait HookService: Send + Sync {
    async fn run(&self, request: HookRequest) -> HookReport;

    fn configured_hooks(&self, point: HookPoint) -> Vec<String> {
        Vec::new()
    }
}
```

`HookRequest` carries the `HookPoint`, the immutable `FiringView` (node with
its `meta`, visit, attempt, inputs, run context, `BranchRole`), the outcome
where one exists, the selected routes at `RouteSelected`, and an opaque
`payload` for the points that carry one (the tool boundary, `ScopeReady`,
`ForkCompleted`, the two run-level points). `HookReport` carries the
decision, one `HookRun` per hook that ran or could not run (`executed`,
`skipped`, `failed_open`, `unsupported`) with what the hook's own model and
tool work cost (`HookUsage`: requests made, tool calls started, the
backend's token counts, cost, timings), fail-open warnings, and the
activity of the hooks' own agents (`HookActivity`: the hook operation, the
backend, the backend's event envelope), which is not part of the report's
own record but recorded beside it.
`configured_hooks` names the hooks configured for a point, matcher aside; a
caller that cannot serve a boundary (the ACP backend, which has no post-tool
boundary and sees only the tool calls that ask for permission) uses it to
warn about each configured hook it cannot run. The default answer is none:
such a caller then warns about nothing and still asks the service at every
boundary it does have.

Exactly one service is installed per run: `NoHooks` by default, the local
executor in the standalone runner (`fabro_steps::hooks::LocalHooks`, installed
by `fabro_steps::register` when no `Runtime::hooks` is installed yet), the
host's own when embedded. The service owns configuration, matching, placement
(host or sandbox), timeouts, and how an executor's result becomes a decision
(a command's exit code, HTTP, prompt and agent hooks). Every caller reaches
the service the same way: the adapter holds it, and every step that asks a
point itself looks it up as the `HookServiceHandle` capability. Nothing in
Petri consults hook configuration for a workflow point by any other route, so
no hook runs twice whoever serves it, and a replacement service receives
every point.

A host replaces the service by installing its own `Runtime::hooks` (normally
`HookAdapter::new(service)`, or a wrapper around it) and registering
`HookServiceHandle(service)` before `fabro_steps::register` runs; the local
service is then not installed at all. A host that wants the local service
under its own `ExecutionHooks` (a pause, a marker note at every point) calls
`register` first and wraps `Runtime::installed_hooks()`; it never constructs
the local service itself. Both are exercised: the first by
`embedding::a_hook_service_runs_each_hook_once_at_its_point` and the two
`a_replacement_service_*` tests of `petri-fabro-steps::hooks`, the second by
`embedding_readiness.rs`.

## Points and decisions

| `HookPoint` | Driven by | Decisions consumed |
| --- | --- | --- |
| `BeforeVisit` | `HookAdapter::before_attempt`, first attempt of a firing; for a node whose meta says `admission_hooks = "step"`, the node's own step instead, once its environment is in place (below) | `Skip`, `Block` |
| `BeforeAttempt` | `HookAdapter::before_attempt`, every attempt; for an `admission_hooks = "step"` node, its step, after `BeforeVisit` | `Skip`, `Block` |
| `Retrying` | `HookAdapter::before_attempt`, attempts after the first | none |
| `AfterAttempt` | `HookAdapter::prepare_result` | `Adjust` |
| `AfterVisit` | `HookAdapter::after_record` | none |
| `RouteSelected` | `HookAdapter::transition` | `Override`, `Block` |
| `ForkStarted` | the fork node's step (`fabro/fork`), through the `HookServiceHandle`, once per fork visit before any branch; the view is the fork node's | none |
| `ForkCompleted` | the fan-in step (plain, synthetic, or prompted), through the `HookServiceHandle`, once every branch is in and before the results are published; the view is the fan-in's, the payload `ForkCompletedPayload { fork }` names the fork node | none |
| `ScopeReady` | the first step to run in a scope's environment (`fabro/stage` at the root workflow's `start`), through the `HookServiceHandle`, once per run, after the checkout seeded the workspace; the view is the step's, the payload `ScopeReadyPayload { scope, workspace }` | `Block` |
| `RunStarted` | the same step, right after `ScopeReady`, once per run; the view is the step's, no payload | `Block` |
| `RunFinished` | `HookAdapter::run_finished`, from the driver that owns the run (a bare driver, or the coordinator's root invocation) at a terminal exit, before any environment is released; no firing view, payload `RunFinishedPayload { status, failure }`; the report comes back as a note the coordinator records at run level | none |
| `ScopeReleased` | `HookAdapter::scope_released`, from the driver just before a scope's own environment is released (an inherited sandbox's release reports nothing); a release that is part of the run's end waits for `RunFinished`; no firing view, payload `ScopeReleasedPayload { scope, outcome }`; the report comes back as a note the coordinator records at run level | none |
| `BeforeToolUse`, `AfterToolUse`, `AfterToolFailure` | the agent backend's tool middleware (native) or permission handler (ACP), through the `HookServiceHandle`, at the actual tool boundary | `Block` at `BeforeToolUse` |

A decision a point does not consume is ignored; the report is still recorded.
Fabro's `stage_start` maps to `BeforeAttempt` (it runs before every attempt,
retries included); `stage_complete`/`stage_failed` to `AfterVisit`;
`stage_retrying` to `Retrying`; `edge_selected` to `RouteSelected`;
`parallel_start`/`parallel_complete` to `ForkStarted`/`ForkCompleted`;
`sandbox_ready` to `ScopeReady`; `run_start` to `RunStarted`; `run_complete`
and `run_failed` to `RunFinished` (by the run's final status, neither for a
cancelled run, as Fabro's `on_run_end` does); `sandbox_cleanup` to
`ScopeReleased`.

The fork points are asked by steps, not the adapter, because the driver
cannot see every fork at admission: a `for_each` fork has one routing group
and expands after its node ran. The fork step is the parallel node itself and
runs exactly once per fork visit before any branch; the fan-in runs exactly
once when every branch is in. A skipped fork node runs no fork step, so it
announces no `parallel_start`, as Fabro's skipped parallel handler does not.

`ScopeReady`, `RunStarted` and the start stage's admission are asked by the
stage step because the driver admits the first firing of a scope before the
scope's environment exists, and a hook placed in the sandbox needs it. The
frontend marks the start node `admission_hooks = "step"` in its meta
(`ir::placeholder::ADMISSION_HOOKS_META`); the adapter then admits it without
asking, and the step asks, with the sandbox in place, in Fabro's order:
`ScopeReady`, `RunStarted`, `BeforeVisit`, `BeforeAttempt`. A `Block` at any
of them fails the stage (`hook_blocked`) and stops the run before work; a
`Skip` at the two admission points skips the stage. Only the root workflow's
`start` asks the two run-level points (its config carries the run's hook
list); a nested workflow's `start` asks its own admission only, so a run
delivers `ScopeReady` and `RunStarted` once. A host whose own pipeline
already ran the `sandbox_ready` and `run_start` phases answers `Proceed` at
these two points without running hooks: Petri asks once, the service decides
whether anything runs, and nothing runs twice. The reports of every
step-asked point ride the public stream as `fabro.hook` events on the
asking firing (`EVENTS.md`), where the adapter's ride as `hook` notes.

The two run-level end points carry no firing, so the driver records nothing
for them itself: `ExecutionHooks::run_finished` and `scope_released` return
the adapter's `hook` notes, the driver hands them to its host in
`ExecutionReport::run_notes` (masked, `run_finished` first, then each
release), and the coordinator appends one `run.note.recorded` per
note before it records `run.finished`. `replay_run` carries them as
`run.note.recorded` events (`parsed.note.kind = hook`) with no subject and the
execution named, and
`petri inspect` lists them under `notes`. The record is additive to
coordinator format version 2: a log without it replays as before. A
`sandbox_cleanup` report of an environment released after the run's finish
(a prune after a crash) has no driver and is not recorded.

`ExecutionHooks` wrappers (the control service's pause hooks are one) must
forward `run_finished` and `scope_released` as they forward the other points
and return the inner notes, or the run-level hooks never run or never record.

## Recording

Every non-silent report is recorded as a `hook` note on the firing, so it is in
the engine log, replayed, and visible in the public event stream as a
`step.progress.recorded` whose `parsed.note.kind` is `hook`. The note carries the point, the decision, each hook's
name, state, duration, message and usage, and the fail-open warnings.
Executed hooks and hooks that could not run are told apart by
`HookRun::state`.

A hook that runs an agent (the local agent hook) owns model requests, tool
calls and an event stream of its own. They go on the record under the hook's
identity, never as the stage's: `HookReport::notes` yields one
`hook.activity` note per agent event (`HookActivity { hook: { point, hook },
backend, envelope }`), then the `hook` note; the adapter records them at the
driver's points, and a step that asks a point itself records the same
activity notes before its `fabro.hook` event (`fabro_steps::hooks::record_report`).
The projector reads each note as `parsed.hook_activity`, apart from the
stage's own backend events, so a consumer summing a stage's agent activity
or its `pebble.usage` never counts a hook's work, and `HookRun::usage` sums
the hook's requests, tool calls, tokens, cost and timings (a prompt hook's
one request; an agent hook's turns, from the prompt's report, including an
interrupted prompt's). The local agent hook installs a Petri event sink for
this; the agent's shutdown flushes the last events before the record is
taken. The reference discards all of this; Petri records it as an
observability improvement (readiness item 7). Fabro's hook-recursion rule
holds: a hook's agent runs without the tool-hook middleware and under the
recursion guard, so its own tool calls fire no hooks, and its activity is
the hook's, not a stage's (`agent_hooks_investigate_the_workspace_then_decide`,
`an_agent_hooks_activity_is_kept_apart_from_the_stages_own`).

## Timeouts, errors, cancellation

The service applies its own timeout and fail-open policy and returns a report;
it never returns an error. Dropping the future is the cancellation signal: a
root kill aborts the awaiting callback, and the driver then records the
original result. A service must not call back into the run.

A service that owns work a dropped future cannot finish — the local agent
hook's tool process in the sandbox and its agent's tasks — keeps that work on
a task of its own, so the drop still leaves an owner. The local agent hook
does this: a timeout cancels the prompt, waits for the running tool to stop
(the environment's TERM, the scope's grace, then a KILL) and for the agent's
tasks to join, and only then returns its fail-open report; a dropped hook
future cancels the same way through a guard, and the owner finishes the same
cleanup on its own, bounded by the grace plus a fixed margin
(`petri-fabro-steps::hooks::an_agent_hook_timeout_stops_its_tool_before_failing_open`,
`a_cancelled_run_stops_an_agent_hooks_running_tool`).
