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
}
```

`HookRequest` carries the `HookPoint`, the immutable `FiringView` (node with
its `meta`, visit, attempt, inputs, run context, `BranchRole`), the outcome
where one exists, the selected routes at `RouteSelected`, and an opaque
`payload` for tool-boundary points. `HookReport` carries the decision, one
`HookRun` per hook that ran or could not run (`executed`, `skipped`,
`failed_open`, `unsupported`), and fail-open warnings.

Exactly one service is installed per run: `NoHooks` by default, the local
executor in the standalone runner (`fabro_steps::hooks::LocalHooks`, installed
by `fabro_steps::register` when no service is installed yet), the host's own
when embedded. The service
owns configuration, matching, placement (host or sandbox), timeouts, and how
an executor's result becomes a decision (a command's exit code, HTTP, prompt
and agent hooks). Nothing else in Petri consults hook configuration for a
workflow point, so no hook runs twice whoever serves it.

## Points and decisions

| `HookPoint` | Driven by | Decisions consumed |
| --- | --- | --- |
| `BeforeVisit` | `HookAdapter::before_attempt`, first attempt of a firing | `Skip`, `Block` |
| `BeforeAttempt` | `HookAdapter::before_attempt`, every attempt | `Skip`, `Block` |
| `Retrying` | `HookAdapter::before_attempt`, attempts after the first | none |
| `AfterAttempt` | `HookAdapter::prepare_result` | `Adjust` |
| `AfterVisit` | `HookAdapter::after_record` | none |
| `RouteSelected` | `HookAdapter::transition` | `Override`, `Block` |
| `ForkStarted` | the local service, from `BeforeAttempt` of a node whose Fabro kind is `parallel` (a static fork or a `for_each` fork), once per fork visit | none |
| `ForkCompleted` | the fan-in step itself (plain, synthetic, or prompted), through the local service, once every branch is in and before the results are published | none |
| `RunFinished` | `HookAdapter::run_finished`, from the driver that owns the run (a bare driver, or the coordinator's root invocation) at a terminal exit, before any environment is released; no firing view, payload `RunFinishedPayload { status, failure }`; the report comes back as a note the coordinator records at run level | none |
| `ScopeReleased` | `HookAdapter::scope_released`, from the driver just before a scope's own environment is released (an inherited sandbox's release reports nothing); a release that is part of the run's end waits for `RunFinished`; no firing view, payload `ScopeReleasedPayload { scope, outcome }`; the report comes back as a note the coordinator records at run level | none |
| `BeforeToolUse`, `AfterToolUse`, `AfterToolFailure` | the agent backend's tool middleware, through the `HookServiceHandle` capability, at the actual tool boundary | `Block` at `BeforeToolUse` |

A decision a point does not consume is ignored; the report is still recorded.
Fabro's `stage_start` maps to `BeforeAttempt` (it runs before every attempt,
retries included); `stage_complete`/`stage_failed` to `AfterVisit`;
`stage_retrying` to `Retrying`; `edge_selected` to `RouteSelected`;
`parallel_start`/`parallel_complete` to `ForkStarted`/`ForkCompleted`;
`run_complete` and `run_failed` to `RunFinished` (by the run's final status,
neither for a cancelled run, as Fabro's `on_run_end` does); `sandbox_cleanup`
to `ScopeReleased`. `run_start` and `sandbox_ready` are driven by the
`fabro/stage` step at `start`, with the sandbox in place. The two run-level
points carry no firing, so the driver records nothing for them itself:
`ExecutionHooks::run_finished` and `scope_released` return the adapter's
`hook` notes, the driver hands them to its host in
`ExecutionReport::run_notes` (masked, `run_finished` first, then each
release), and the coordinator appends one `CoordinatorEvent::RunNote` per
note before it records `RunFinished`. `replay_run` derives them as
`host_note {kind: "hook"}` with no subject and the execution named, and
`petri inspect` lists them under `notes`. The record is additive to
coordinator format version 2: a log without it replays as before. A
`sandbox_cleanup` report of an environment released after the run's finish
(a prune after a crash) has no driver and is not recorded.

`ExecutionHooks` wrappers (the control service's pause hooks are one) must
forward `run_finished` and `scope_released` as they forward the other points
and return the inner notes, or the run-level hooks never run or never record.

## Recording

Every non-silent report is recorded as a `hook` note on the firing, so it is in
the engine log, replayed, and visible as `host_note { kind: "hook" }` in the
public event stream. The note carries the point, the decision, each hook's
name, state, duration and message, and the fail-open warnings. Executed hooks
and hooks that could not run are told apart by `HookRun::state`.

## Timeouts, errors, cancellation

The service applies its own timeout and fail-open policy and returns a report;
it never returns an error. Dropping the future is the cancellation signal: a
root kill aborts the awaiting callback, and the driver then records the
original result. A service must not call back into the run.
