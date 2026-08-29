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
`timeout-minutes`, `continue-on-error` (literal, step and job level — a
tolerated job's failure reports `success` to its dependents and the run, and
does not trigger matrix fail-fast, as on GitHub; its steps' own outcomes stay
as they fell), `strategy.matrix` with
`include`/`exclude`, `fail-fast` and `max-parallel`. `runs-on: ${{ matrix.os }}`
— and any `runs-on` over `matrix` values, `inputs` (a literal call site's
`with:`, or the declared default, which is also how a directly run reusable
file places), the checkout's `github` identity, `fromJSON`, templates and the
documented functions — resolves at lowering, once per leg and per call site,
through the same matrix expansion the engine runs. The identity is the
repository slug from the checkout's `origin` remote — stable across commits,
deliberately never HEAD, and the same value `default_params` hands the run, so
a guard's branch choice and a step's `github.repository` cannot disagree;
without a checkout, guards take their fallback branch. At *run* time,
`default_params` also fills `github.sha`, `github.ref` and `github.ref_name`
from the checkout's actual HEAD — run parameters, recorded in the graph, so
replay is untouched — which is what lets `actions/checkout` fetch a commit
that exists; without a checkout the fixed values (zero sha, `main`) stand. Matrix values resolve
against the same contexts, so a repository-guarded axis is as static as a
literal one; each leg's labels face the same placement
policy as literal labels (Linux labels the executor knows), a rejected leg names
itself without stopping the others, and the per-leg results are preserved on the
job's `start` node. `on:` is accepted as metadata — a local run fires the
workflow directly; `github.event` comes from the run's parameters. `name:` and
`run-name:` — the workflow, job and step display names — are accepted as
metadata too; nodes are named by their ids, so the display names carry no
semantics to drop.

**Steps.** `run:` steps; `uses:` for JavaScript actions (fetched from their
repositories, resolved to a pinned commit at load time, `pre`/`main`/`post`
placed as GitHub orders them, state flowing between phases), for composite
actions (local and remote, inlined), and for Docker container actions —
`uses: docker://image`, and fetched or local actions whose manifest says
`runs.using: docker` with a registry image or a Dockerfile (built once per
pinned commit and cached; a local action's rebuilds once per job). One
container per phase invocation against the runner's daemon, the job workspace
mounted, declared inputs as `INPUT_*`, the manifest's `args`/`env`
expression-interpolated over `inputs`, `entrypoint` and `with.args`/
`with.entrypoint` overrides, `pre-entrypoint`/`post-entrypoint` placed exactly
as JavaScript actions' phases; `with:` inputs with declared defaults; step ids
and `steps.<id>.outputs`. A missing required action input warns and runs, as
GitHub's runner does.

**Local checkout.** A supportable `actions/checkout` call substitutes at
lowering for the `github/checkout` step: the workspace materializes from the
run's own repository — the committed HEAD as a depth-1 local clone, plus the
uncommitted tracked diff and the untracked-but-not-ignored files, `.git` and
mode bits included — offline and token-less, in host and containerized jobs
alike. *The tree you have*, which is what a local run should test, and a
deliberate delta from GitHub (which fetches the pushed commit). Supportable
means: default inputs, plus a literal `path:`; `fetch-depth` and
`persist-credentials` accepted and ignored (local-clone policy); a literal
`repository:`/`ref:` equal to the checkout's own still substitutes, since real
workflows spell the default out. Anything else — another repository, an
expression or non-matching value, `submodules:`, a `token:` — falls through to
the real action, which run identity (`github.sha` from HEAD) makes work for
pushed state given a credential. The substitution is lowering-visible, never a
silent runtime intercept, and `PETRI_REAL_CHECKOUT` (or
`with_checkout_substitution(false)`) turns it off. A repository root without
`.git` materializes as a plain tree copy. The snapshot's git *shape* matches
what GitHub's checkout leaves: a branch run (`github.ref` under `refs/heads/`)
has that branch checked out with a matching `refs/remotes/origin/<branch>` —
even when the source sits detached, as a corpus pin does — and `origin` is set
to `<server_url>/<repository>` where the repository is named, so an action
asking an ordinary git question (the current branch, a rev-parse, a diff
against a base) sees what it expects rather than the scratch clone's
`file://` host path.

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

**Containers.** `container:` on a job runs its steps in a container from the
image, with the workspace bind-mounted. `container.env` configures the
container (job and step env win over it; a secret there pushes down into the
steps like a job-env secret). `container.options` — and a service's — pass
through to the engine as raw flags, split as GitHub splits them.
`container.credentials` (and a service's): the username resolves at lowering,
the password must be a whole `${{ secrets.NAME }}` reference — the graph
carries the *name*, and the executor logs in inside acquire with an isolated
Docker config, so no value enters the graph, the log, or the user's own
credential store. An expression-valued image or username resolves at lowering
through the static contexts (`inputs`, the checkout's `github` identity);
`matrix` cannot vary a per-job image — every leg shares the scope — and stays
the `container.expression` rejection with the reason named.

**Service containers.** `services:` on a job: sidecar containers with the
job's lifetime, on a per-job network, each reachable by its service name —
ports published to the host when the job runs on the host, container-to-
container when the job is containerized. Image, `env`, `ports`, and `options`
(raw engine flags, health checks included) map through; acquire waits for
Docker-reported health — the image's `HEALTHCHECK`, or a `--health-cmd` in the
options; a service with no check is ready when running — and a service that
never gets healthy fails the job's environment routably, naming itself. Torn
down with the job on success, failure, cancel, and crash.

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

**The GitHub API.** Petri neither proxies nor blocks it (v1 stance, decided
2026-08-28): `github-script`, `gh` steps and API-calling actions hit the real
API with the ambient token when one exists (`$GITHUB_TOKEN`, else a logged-in
`gh`). Without one, `github.token` is the **empty string** (with a warning at
run start), never a missing secret: the toolkit treats an empty token as "no
auth", so actions' API calls go anonymous — the setup-* version manifests and
public reads work (rate-limited at GitHub's anonymous tier), writes fail with
the API's own error. The real `actions/checkout` is the exception — it
requires a token input and cannot run token-less — which is one more reason
the local-checkout substitution above exists.
**A local run with a real token can mutate — comment, tag, release — exactly
as the workflow says.** Prefer a fine-grained read-only token when running
workflows you did not write; the corpus sweep runs token-less by policy. A
default-deny proxy stays future work if real demand appears.

**Known deltas from GitHub.** The local executor emulates a Linux runner
(`ubuntu-*` labels) on this machine; `runner.os` reports the actual host.
JavaScript actions and `hashFiles` need `node` on `PATH`; Docker container
actions (and `container:` jobs) need a reachable Docker daemon — its absence is
one routable failure on the step or scope, never a run abort. **Artifacts and the cache run against local
stand-ins**: every run gets its own ObjectService — the results backend the
2026 toolkit speaks (`ACTIONS_RESULTS_URL`/`ACTIONS_RUNTIME_TOKEN`,
`ACTIONS_CACHE_SERVICE_V2`, twirp plus signed blob URLs). Artifacts store
under `<run_dir>/artifacts`, released with the run; cache entries live in the
host's persistent store (`$PETRI_STORE`, default `~/.cache/petri/store`; 10
GiB, pruned LRU on write) and outlive the run — save in one run, restore in
the next. Entries are immutable per `(key, version)` as on GitHub; lookups
match the exact key then each restore key as a prefix, newest first.
**GitHub's branch scoping is deliberately ignored locally**: one machine, one
store. Upload and download flow across jobs, host and containerized alike
(containers reach the service through `host.docker.internal`, which the
executor guarantees); nothing leaves the machine, and cross-**run** artifact
reads (`download-artifact` with `run-id:`) go to the real REST API and need
real credentials. The tool cache is persistent too: host jobs get
`RUNNER_TOOL_CACHE` pointed at `<store>/toolcache/<os>` (per-OS, since the
toolkit's layout has no OS segment), so setup-* stop re-downloading every
run; a container image that names its own populated tool cache keeps it —
mounting an initially-empty persistent cache over `/opt/hostedtoolcache`
would remove tools — and a bare image falls back to the per-run workspace.
`shell: bash` invokes `bash -eo pipefail -c`, not GitHub's
`--noprofile --norc` file invocation; JavaScript actions run through `bash -c
exec node`, so a container image that has `node` but no `bash` cannot run them
(GitHub execs `node` directly). Environment names that are not valid shell
identifiers — the toolkit's own `INPUT_INCLUDE-HIDDEN-FILES` shape — survive
only where the wrapper shell is bash: petri prefers bash for its process
wrappers exactly so they do, and a busybox-only container drops them (dash
semantics). Placement statics keep the fixed zero
`github.sha` while the run's parameters carry the checkout's honest HEAD, so a
*condition* on `github.sha` sees the zero sha where placement resolves it and
the real commit at run time — placement never reads HEAD by design.

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
| `runs_on.expression` | `runs-on` the lowering cannot resolve | `matrix`, `inputs` and the checkout's `github` identity resolve per leg (above); what remains reads `needs`, an input the call site computes at run time, or a matrix that stays dynamic under those contexts. |
| `workflow_call.matrix` | A matrix inside a matrix workflow call | The engine expands one region at a time; clones cannot expand again. Nested expansion is an engine feature to design, not a frontend gap. |
| `container.expression` | A container or service value the static contexts cannot resolve [0] | `inputs` and the checkout's `github` identity resolve at lowering (above); what remains reads `matrix` (per-scope values cannot vary per leg), a run-time context, or a dynamic input. |
| `container.credentials` | A registry password that is not a `${{ secrets.* }}` reference [0] | Only a secret name may cross into the graph; a computed password would need a resolution seam inside acquire. |
| `container.ports`, `container.volumes`, `services.secret_env`, `services.volumes` | Container and service corners [0] | Job-container port mappings and volumes name runner-machine resources to map; secret-valued service env needs a resolution point inside acquire. |
| `action.local_missing` | `uses: ./x` that exists only after checkout [4] | Defer the manifest read to run time. |
| `timeout.expression`, `continue_on_error.expression`, `strategy.fail_fast.expression`, `strategy.max_parallel.expression`, `strategy.job_total.dynamic`, `env.expression` | Expression-valued control fields [3] | Evaluate at lowering where the value is static, reject the rest. |
| `step.background` | Background steps [2] | GitHub shipped these June 2026. |
| `action.nested_local` | `./` actions inside a fetched composite or called workflow [2] | Stage the fetched repository so relative references resolve. |
| `job_context` | `job.container` / `job.services` in an expression [0] | Service containers run (above), but their ids, networks and host port mappings are run-time facts the expression environment does not carry yet. Reach a service by its name and declared ports. |
| `yaml.multiline_flow` | YAML reader gap [0] | The residual shape: a flow *item* line at or left of its block parent's indentation. A closer-only line there — the shape the corpus actually had — is re-indented and accepted, and anchors and aliases resolve since the reader grew its own loader. |

Planned on the runtime side (no rejection code — lowered workflows fail at run
time instead): the persistent tool cache mounted into container images that
ship none, the legacy cache v1 REST façade if pressure appears, and
authenticated fetch for private action repositories.

## Out of scope

Rejected with an error, and staying that way.

| Code | What | Why |
|---|---|---|
| `runs_on.windows`, `runs_on.macos` | Windows and macOS runners — named anywhere in the label (`windows-latest`, `namespace-profile-macos-15`) | The local executor emulates Linux runners only. Detected by whole label tokens, so a third-party pool for these platforms is its platform's rejection, not an unknown label. |
| `shell.cmd`, `shell.powershell` | Windows-only shells | Same. |
| `runs_on.group` | `runs-on.group` runner groups | A GitHub-hosted concept; name a label instead. |
| `runs_on.unknown` | Labels that say nothing about their platform (`self-hosted`, `gpu`, vendor pool names) — unless configured | A label naming an Ubuntu or Linux environment in its own tokens (`ubuntu-24.04-xl`, `depot-ubuntu-22.04-16`) places without configuration — the same claim `ubuntu-latest` makes. Opaque labels answer to the runner map (`PETRI_RUNNER_LABELS` for the shipped CLI, `GitHubActions::with_runners` in code). Static and expression-derived labels go through the one check, all of a job's labels must resolve, and an unmapped label is this explicit rejection — never a silently skipped job. Windows and macOS tokens stay their own errors whatever the map says. |
| `secrets.expression` | A secret in a condition (any level), inside a larger output expression, or in a matrix | The docs' availability table gives conditions no `secrets` context, and secrets are absent from the expression environment by construction, so they can never reach the event log. In step config they are supported, and a whole-value secret job output is dropped with `ignored.secret_output` (above). |
| `expression.hashFiles` | `hashFiles` in a job `if:`, an output or a matrix; under another function in a condition; or with computed patterns | Only the step, at spawn, may read the workspace. In a step-level condition it works standing alone or under the comparison and boolean operators (above). |
| `expression.workspace` | `github.workspace` under a function in a condition | The path is runner-side truth the step substitutes at spawn; standing alone or under the comparison and boolean operators it resolves step-side, but under other functions the engine would evaluate over the unresolved marker. Read `GITHUB_WORKSPACE` in the step instead. |
| `runs_on.callee_input` | A reusable file, lowered standalone, whose `runs-on` reads a `workflow_call` input with no default | Only a caller can place such a file, and every call site does (per-call-site resolution, above); standalone there is no runner to place by construction. The corpus keeps these out of the compatibility denominator ("callee only"), the way Windows/macOS workflows leave it. |
| `action.remote` | A remote action with no action source configured, or one the source does not cover | Not a feature gap: the hint points at refreshing the source, which may add the reference. Authenticated fetch for private repositories is planned runtime work, above. |
| `action.upstream_gone` | A remote action whose repository is gone upstream (private or removed, recorded at snapshot refresh) | The workflow is broken on GitHub itself — the corpus has exactly one — so no refresh, and no feature work here, can change it. Kept out of the compatibility denominator ("broken upstream"). |
