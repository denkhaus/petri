# GitHub Actions support

What this component supports, what it plans to, and what is out of scope — the
declared counterpart to the empirical [`corpus/REPORT.md`](corpus/REPORT.md),
which records what today's code actually does to 316 real workflows. A harness
test checks that every `unsupported.*` and `ignored.*` code the corpus run emits
appears in this file, so the two cannot drift apart silently.

The standing rule: nothing is parsed-and-dropped. A construct is **supported**,
**ignored with a warning** naming what was ignored, or **rejected** with a
specific code and a hint. `gha.*` codes are malformed-file errors (a missing
`runs-on`, a bad `needs`), not feature status, and are not listed here.

## Supported

**Workflow structure.** `jobs`, `needs`, job and step `if:` with the status
functions (`success`, `failure`, `always`, `cancelled` — cleanup steps really run
after a cancel), `env` at workflow/job/step level, `defaults.run`, job `outputs`,
`timeout-minutes`, `continue-on-error` (literal), `strategy.matrix` with
`include`/`exclude`, `fail-fast` and `max-parallel`. `runs-on: ${{ matrix.os }}`
— and any `runs-on` over `matrix` values, `fromJSON`, templates and the
documented functions — resolves at lowering, once per leg, through the same
matrix expansion the engine runs; each leg's labels face the same placement
policy as literal labels (Linux labels the executor knows), a rejected leg names
itself without stopping the others, and the per-leg results are preserved on the
job's `start` node. `on:` is accepted as metadata — a local run fires the
workflow directly; `github.event` comes from the run's parameters.

**Steps.** `run:` steps; `uses:` for JavaScript actions (fetched from their
repositories, resolved to a pinned commit at load time, `pre`/`main`/`post`
placed as GitHub orders them, state flowing between phases) and for composite
actions (local and remote, inlined); `with:` inputs with declared defaults;
step ids and `steps.<id>.outputs`.

**Reusable workflows.** A `uses:` job calls another workflow — local
(`./.github/workflows/x.yml`, or GitHub's `$/` same-repository shorthand) or
pinned remote (`owner/repo/.github/workflows/x.yml@ref`, fetched through the
action source) — and the callee's jobs inline under the call's name, each with
its own scope and placement. Typed inputs (`string`, `boolean`, `number`,
`choice`, `environment`) validate statically where the value is literal and
coerce with the engine's builtins where it is not; `required` and declared
defaults are enforced; unknown `with:` keys and undeclared secrets are errors,
as on GitHub. `secrets: inherit` and explicit `secrets:` maps are pure renames
at lowering — no value, and no ungranted name, crosses the boundary; the log
never sees either. `GITHUB_TOKEN` crosses every boundary unmapped, as on
GitHub. `workflow_call.outputs` lower over the `jobs.*` context and surface to
the caller as `needs.<call>.outputs.*`; a skipped call skips the whole callee,
`always()` jobs included; a matrix on the call fans the entire callee out per
leg, `with:` evaluated per leg. Calls nest to GitHub's depth limit with cycle
diagnostics. `workflow_dispatch` inputs — and a reusable file run directly —
bind `inputs` from the run's parameters (`github.event.inputs`) through the
same typed model.

**Shells.** `bash`, `sh`, `python`, `pwsh`, and any custom template containing
`{0}` (`bash -el {0}`, `/usr/bin/env bash {0}`): the script is written to a file
and the template runs over it, as GitHub does. Whether the interpreter exists is
the job environment's business.

**Expressions.** The GitHub expression grammar over the `github`, `env`, `vars`,
`runner`, `matrix`, `needs`, `steps` and `inputs` contexts, and the
documented functions. `hashFiles(...)` with literal patterns works in step config
(`run:`, `env:`, `with:`) and in step-level conditions (`if:`, `pre-if`,
`post-if`), resolved against the workspace at spawn with GitHub's hash. Secrets —
`secrets.*` and `github.token` — may appear anywhere in step config; the value is
spliced in at spawn and never enters the graph or the log.

**Conditions.** The docs' context-availability matrix is the compatibility
target. A step-level condition sees `github`, `needs`, `strategy`, `matrix`,
`job`, `runner`, `env`, `vars`, `steps`, `inputs`, the status functions, and
`hashFiles` — every step-level condition lowers to a gate the step evaluates at
spawn, so `env.*` sees what earlier steps appended through `GITHUB_ENV`, as it
does on GitHub's runner. A job-level `if:` sees `github`, `needs`, `vars`,
`inputs` and the status functions, and is evaluated by the engine — the same
role GitHub's server plays: no secrets, no workspace, no env files. The docs do
not give conditions the `secrets` context at any level, and neither does this
runner, even where GitHub accidentally accepts more; pass the secret through an
environment variable and test `env.NAME` in the condition.

**Runner contract.** `GITHUB_ENV`, `GITHUB_PATH`, `GITHUB_STATE`,
`GITHUB_OUTPUT`, `GITHUB_STEP_SUMMARY` (heredoc syntax included), the `GITHUB_*`
and `RUNNER_*` variables, `GITHUB_EVENT_PATH`, and the `::` workflow commands:
`set-output`, `save-state`, `add-mask`, `error`/`warning`/`notice`, `group`,
`debug`, `echo`, `stop-commands`, with `set-env`/`add-path` gated behind
`ACTIONS_ALLOW_UNSECURE_COMMANDS` as on GitHub. `GITHUB_TOKEN` comes from the
environment or a logged-in `gh`.

**Known deltas from GitHub.** The local executor emulates a Linux runner
(`ubuntu-*` labels) on this machine; `runner.os` reports the actual host.
JavaScript actions and `hashFiles` need `node` on `PATH`. Actions that call
GitHub's hosted backends (artifact upload/download, the cache service) run their
real code and fail at the HTTP call — local stand-ins are planned, below.
`shell: bash` invokes `bash -eo pipefail -c`, not GitHub's
`--noprofile --norc` file invocation.

## Ignored, with a warning

| Code | What, and why ignoring is sound |
|---|---|
| `ignored.concurrency` | `concurrency:` is cross-run mutual exclusion; a single local run has nothing to race. Cross-run semantics stay with a multi-run driver layer (decision D2). |
| `ignored.environment` | `environment:` names a deployment target whose enforcement — approvals, protection rules, wait timers, environment-scoped secrets and variables — lives on GitHub's servers. The name, URL and `deployment` flag (expressions stay as written) are preserved on the job's `start` node; nothing pretends protection rules ran, and environment-scoped secrets stay unavailable. |
| `ignored.permissions` | `permissions:` configures the GitHub-hosted token; it grants nothing locally. |
| `ignored.secret_output` | A job output that would carry a secret is dropped, as GitHub drops it (with its "skip output" warning). |
| `action.nested_lifecycle` | A nested action's `pre`/`post` inside a composite does not run (its `main` does), as a warning on the composite. |

## Planned

Rejected today with the code shown; the intent is to support them. Corpus
workflow counts in brackets rank the pressure.

| Code | Feature | Shape of the plan |
|---|---|---|
| `runs_on.expression` | `runs-on` the lowering cannot resolve | `matrix`-valued expressions resolve per leg (above); what remains reads `inputs` (statically known for a literal call — extending the per-leg resolver to it is the next step), run-time contexts (`github`, `needs`), or a dynamic matrix. |
| `workflow_call.matrix` | A matrix inside a matrix workflow call | The engine expands one region at a time; clones cannot expand again. Nested expansion is an engine feature to design, not a frontend gap. |
| `action.docker`, `services`, `container.expression`, `container.options` | The Docker tier [18, 5, 4] | Docker container actions, service containers, container options — shelling out to `docker` the way actions are fetched with `git`. |
| `action.local_missing` | `uses: ./x` that exists only after checkout [4] | Defer the manifest read to run time. |
| `timeout.expression`, `continue_on_error.expression`, `strategy.fail_fast.expression`, `strategy.max_parallel.expression`, `strategy.job_total.dynamic`, `env.expression` | Expression-valued control fields [3] | Evaluate at lowering where the value is static, reject the rest. |
| `step.background` | Background steps [2] | GitHub shipped these June 2026. |
| `action.nested_local` | `./` actions inside a fetched composite or called workflow [2] | Stage the fetched repository so relative references resolve. |
| `yaml.multiline_flow` | YAML reader gap [0] | The residual shape: a flow *item* line at or left of its block parent's indentation. A closer-only line there — the shape the corpus actually had — is re-indented and accepted, and anchors and aliases resolve since the reader grew its own loader. |

Planned on the runtime side (no rejection code — lowered workflows fail at run
time instead): local stand-ins for the artifact and cache services, and
authenticated fetch for private action repositories.

## Out of scope

Rejected with an error, and staying that way.

| Code | What | Why |
|---|---|---|
| `runs_on.windows`, `runs_on.macos` | Windows and macOS runners | The local executor emulates Linux runners only. |
| `shell.cmd`, `shell.powershell` | Windows-only shells | Same. |
| `runs_on.group` | `runs-on.group` runner groups | A GitHub-hosted concept; name a label instead. |
| `runs_on.unknown` | Labels the runner map cannot place (`self-hosted`, third-party pools) — unless configured | The map ships knowing the `ubuntu-*` labels; configuration (`PETRI_RUNNER_LABELS` for the shipped CLI, `GitHubActions::with_runners` in code) adds labels that name Linux environments this machine can stand in for. Static and expression-derived labels answer to the same map, all of a job's labels must resolve, and an unmapped label is this explicit rejection — never a silently skipped job. Windows and macOS labels stay their own errors whatever the map says. |
| `secrets.expression` | A secret in a condition (any level), inside a larger output expression, or in a matrix | The docs' availability table gives conditions no `secrets` context, and secrets are absent from the expression environment by construction, so they can never reach the event log. In step config they are supported, and a whole-value secret job output is dropped with `ignored.secret_output` (above). |
| `expression.hashFiles` | `hashFiles` in a job `if:`, an output or a matrix; under another function in a condition; or with computed patterns | Only the step, at spawn, may read the workspace. In a step-level condition it works standing alone or under the comparison and boolean operators (above). |
| `action.remote` | A remote action with no action source configured, or one the source cannot serve | Not a feature gap, and the hint says which case it is: a reference the source does not cover points at a refresh; one whose refresh met a terminal upstream answer (a private or removed repository — the corpus has exactly one) carries that recorded error, because no refresh will help. Authenticated fetch for private repositories is planned runtime work, above. |
