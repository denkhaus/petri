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
executor in the standalone runner, the host's own when embedded. The service
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
| `ForkStarted`, `ForkCompleted` | not yet driven; reserved for the fork and join views | none |
| `BeforeToolUse`, `AfterToolUse`, `AfterToolFailure` | the agent backend's tool middleware, through the `HookServiceHandle` capability, at the actual tool boundary | `Block` at `BeforeToolUse` |

A decision a point does not consume is ignored; the report is still recorded.
Fabro's `stage_start` maps to `BeforeAttempt` (it runs before every attempt,
retries included); `stage_complete`/`stage_failed` to `AfterVisit`;
`stage_retrying` to `Retrying`; `edge_selected` to `RouteSelected`;
`run_start`, `run_complete`, `run_failed`, `sandbox_*` are run-level points the
host drives from the coordinator lifecycle, outside the per-firing adapter.

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
