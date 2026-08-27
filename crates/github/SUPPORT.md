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
`include`/`exclude`, `fail-fast` and `max-parallel`. `on:` is accepted as
metadata — a local run fires the workflow directly; `github.event` comes from the
run's parameters.

**Steps.** `run:` steps; `uses:` for JavaScript actions (fetched from their
repositories, resolved to a pinned commit at load time, `pre`/`main`/`post`
placed as GitHub orders them, state flowing between phases) and for composite
actions (local and remote, inlined); `with:` inputs with declared defaults;
step ids and `steps.<id>.outputs`.

**Shells.** `bash`, `sh`, `python`, `pwsh`, and any custom template containing
`{0}` (`bash -el {0}`, `/usr/bin/env bash {0}`): the script is written to a file
and the template runs over it, as GitHub does. Whether the interpreter exists is
the job environment's business.

**Expressions.** The GitHub expression grammar over the `github`, `env`, `vars`,
`runner`, `matrix`, `needs`, `steps` and (in actions) `inputs` contexts, and the
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
| `workflow_call` | Reusable workflows [92] | Resolve and inline the called workflow, as remote composites are inlined today. |
| `inputs`, `workflow_dispatch.inputs` | `inputs` context, dispatch inputs [70, 46] | Falls out of reusable-workflow support plus run parameters. |
| `runs_on.expression` | `runs-on: ${{ matrix.os }}` [47] | Split the matrix into one job per leg in the frontend. |
| `action.docker`, `services`, `container.expression`, `container.options` | The Docker tier [18, 5, 4] | Docker container actions, service containers, container options — shelling out to `docker` the way actions are fetched with `git`. |
| `action.local_missing` | `uses: ./x` that exists only after checkout [4] | Defer the manifest read to run time. |
| `timeout.expression`, `continue_on_error.expression`, `strategy.fail_fast.expression`, `strategy.max_parallel.expression`, `strategy.job_total.dynamic`, `env.expression` | Expression-valued control fields [3] | Evaluate at lowering where the value is static, reject the rest. |
| `step.background` | Background steps [2] | GitHub shipped these June 2026. |
| `action.nested_local` | `./` actions inside a fetched composite [2] | Stage the composite's repository so relative references resolve. |
| `yaml.anchors`, `yaml.multiline_flow` | YAML reader gaps [3] | Library limitations of the positional reader. |

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
| `runs_on.unknown` | Labels the executor cannot place (`self-hosted`, third-party pools) | Placing them is a driver decision, not a frontend guess. The known-label list can grow. |
| `secrets.expression` | A secret in a condition (any level), inside a larger output expression, or in a matrix | The docs' availability table gives conditions no `secrets` context, and secrets are absent from the expression environment by construction, so they can never reach the event log. In step config they are supported, and a whole-value secret job output is dropped with `ignored.secret_output` (above). |
| `expression.hashFiles` | `hashFiles` in a job `if:`, an output or a matrix; under another function in a condition; or with computed patterns | Only the step, at spawn, may read the workspace. In a step-level condition it works standing alone or under the comparison and boolean operators (above). |
| `action.remote` | A remote action with no action source configured, or one the source cannot serve | Not a feature gap: configure an action source (the distribution ships one), or refresh the corpus snapshot. |
