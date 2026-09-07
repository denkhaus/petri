# The Fabro execution compatibility contract

This page freezes what Petri promises for Fabro workflows, against one Fabro
revision, for one declared set of complete workflow bundles. It is the
contract the black box battery and the differential comparison test against.
`crates/fabro/FORMAT.md` says how each construct lowers; this page says which
constructs the required bundles use, and whether each one is supported, a
tracked defect, an accepted migration, or an explicit exclusion.

## Reference revision

| What | Value |
|---|---|
| Fabro repository | `fabro-sh/fabro` |
| Reference commit | `b6482910e517d00dfc3c4a2f2d3e417c9348f7f6` (committed 2026-09-05T18:05Z) |
| Where it lives | `refs/pull/844/head`; not on `main` at freeze time |
| Pin file | `crates/fabro/corpus-pin.txt` (the `# ref:` line names the pull ref) |
| Fabro version string | `fabro 0.347.0-nightly.0 (b648291 ...)` |
| Fixtures | `crates/fabro/oracle/expected/*.json`, each with `fabro_commit` equal to the pin |

Rules:

- `scripts/corpus-fetch-fabro.sh` fetches the pinned commit. When the remote
  refuses a bare SHA it fetches the `# ref:` and fails unless that ref still
  resolves to the pin.
- `crates/fabro/acceptance/tests/routing.rs` rejects a fixture whose
  `fabro_commit` differs from the pin. Fixtures and parity runs always use one
  reference version.
- `scripts/oracle-regenerate.sh` builds `fabro` from the fetched checkout into
  `crates/fabro/corpus/fabro-target/` and drives it as a subprocess. The
  harness checks the binary reports the pinned short SHA. A `fabro` on `PATH`
  is never used.
- Petri never links a Fabro crate. `crates/petri/lib/tests/fabro_dependencies.rs`
  scans the full `cargo metadata` resolve graph, every dependency kind, and
  nested manifests and build scripts. `crates/petri/cli/tests/standalone.rs`
  runs the shipped binary with no `fabro` on `PATH`.

## The parity harness

`crates/fabro/oracle/harness/` runs the shared oracle cases through the pinned
Fabro binary and public interfaces only:

| Need | Public interface used |
|---|---|
| A Fabro server | `fabro server start --foreground --no-web --bind 127.0.0.1:<port> --storage-dir <dir> --config <settings.toml>`, dev-token auth, `[environments.local] provider = "local"` |
| Run creation | `fabro run --detach --environment local --provider openai --json <file>` in a fresh git repository; a fake `OPENAI_API_KEY` secret satisfies default model selection and is never called |
| Scripted agent and prompt stages | `backend="acp"`, `acp.command` launching `scripted_acp_agent.py`; the agent's text ends with Fabro's routing directive JSON (`outcome`, `failure_reason`, `preferred_next_label`, `suggested_next_ids`, `context_updates`) |
| Scripted human gates | `GET /api/v1/runs/{id}/questions`, `POST /api/v1/runs/{id}/questions/{qid}/answer` with `{"kind":"selected","option_key":...}` |
| A stage that asks for a retry | a human gate with `timeout="2s"` and no `human.default_choice`; Fabro's handler returns `retry_classify`, the only public path to `retry_requested` |
| Observation | `fabro events <run>` (JSON lines: `stage.completed`, `stage.failed` with `will_retry`, `loop.restart`, `run.completed`, `checkpoint.completed` with `context_values`) |

Cases that a public interface cannot reproduce exactly are recorded in the
fixture's `harness.realization` map so a reviewer can see which node ran as an
ACP agent and which as a human gate. The old in-process generator that linked
`fabro-workflow` is deleted.

## Required bundles

`crates/fabro/acceptance/bundles.lock.json` (schema version 1) is the
replacement set. `scripts/corpus-fetch-fabro-bundles.sh` materializes it on a
clean machine and verifies every digest.

| Bundle | Source and revision | Status | Scenario obligations |
|---|---|---|---|
| `code-review` | `lithoscomputer/code-review` `0c81ffb4f68039ca56842ab3328fde03b9714350` (private; HEAD, clean) | required | empty diff; findings in multiple branches; no surviving findings; invalid output repaired; repair exhausted; one failed branch; reverse branch completion; multi-level fan-out |
| `security-review` | `lithoscomputer/security-review` `c14279e9cdac4f7553b5e010718ada1a76e5c1f8` (public; HEAD, clean) | required | no vulnerabilities; several verified; rejected candidate; partial branch failure; malformed response; timeout and cancel |
| `fix-ci` | `veniceai/factory` `1b50f791ac4811aad62788c2a4282a75d1e92422` (private; bundle files match HEAD, other files in that working tree were dirty) | required, blocked | initial check passes; fails then edit passes; repeated failure exhausts budget; provider failure; timeout; cancellation |
| `implement-issue` (with `implement-plan`) | `fabro-sh/fabro` at the pin | required, blocked | child runs; input and model inheritance; multiple manager cycles; stop condition; child failure; parent cancellation |
| `interview` | `fabro-sh/fabro` at the pin | required | scripted choice, refusal, free text; repeated and concurrent questions; child interviews; delayed and withheld reply; invalid, unexpected and unused answers; timeout and cancel; terminal EOF |

"Blocked" names a required bundle whose scenarios cannot run yet. The
blockers are listed per bundle in the lock file. They are not exclusions.

### Inventory and classification

The discovery scan covered `~/p/` with these exclusions: `node_modules`,
`target`, `.git`, `.cargo`, `vendor`, `dist`, `build`. It found 6,727
`.fabro` and `.dot` files with 332 distinct contents. Every file has a
location class and a content class (evidence file: task1 review). Summary:

| Class | Files | Disposition |
|---|---|---|
| Fabro repository checkouts, worktrees and backups (`~/p/fabro-sh/*`) | 5,842 | the pinned corpus covers them; other checkouts are duplicates of other revisions |
| Petri corpus mirrors and probe evidence | 251 | ignored: fetched data and review artefacts |
| Foreman and Conveyor scratch (run dumps, smoke graphs, release repairs) | 241 | generated or stale; excluded |
| Conveyor and its worktrees (`.fabro/workflows/*`) | 137 | valid Fabro variants of the canonical review bundles and Conveyor-only workflows; not in the first set |
| Attractor-era and third-party Attractor implementations | 439 | legacy Attractor; expected `unsupported.attractor` rejections or unrelated |
| Ordinary Graphviz | 6 | excluded |
| `veniceai/factory` workflows other than fix-ci | 9 | valid Fabro; later candidates |
| Canonical review bundles, fix-ci, corpus implement and interview | required set above | |

## Feature matrix

Every construct the required bundles use, with its disposition. "Supported"
means lowering and execution exist and are tested today. "Tracked defect"
names the task in `.ai/plans/fabro-unified-task-list.md` that owns the fix.

### Graph and node constructs

| Feature | Used by | Disposition |
|---|---|---|
| `Mdiamond` start, `Msquare` exit, `rankdir`, `label` | all | supported |
| `parallelogram` command with `script`, `timeout`, `output_schema="routing"`, `stdin_source` | code-review, security-review, fix-ci, implement-plan | supported |
| `stdin_source="context.internal.run_id"` | code-review, security-review | supported: `kv` carries the run id |
| `stdin_source="context.parallel.results"` | code-review, security-review | tracked defect (task 6): branch results lack `context_updates`; see the departure below |
| agent node (`prompt`, `@prompts/*.j2`, `{% include %}`) | all but fix-ci commands | supported |
| `output_schema="@schemas/*.json"`, `output_retries` | code-review, security-review | supported |
| `on_failure="route"`, `"exit"` (node and graph) | code-review, security-review | supported |
| `on_failure="succeed"` | code-review, security-review | supported with Fabro's promotion order: a failure an explicit route matches stays failed, an unmatched failure is promoted and reports `succeeded` (oracle cases `succeed_*`) |
| `max_retries`, `default_max_retries` | code-review, security-review, implement-plan | supported |
| `component` with `for_each`, `max_parallel`, `tripleoctagon` fan-in | code-review, security-review | supported for expansion; `max_parallel` slot lifetime and branch context are tracked defects (task 6) |
| `class` with `model_stylesheet` (including `{% set %}`, `{% if %}`, `inputs.*`) | code-review, security-review, implement-issue | supported |
| `default_fidelity`, `fidelity="truncate"`, `fidelity="summary:high"` | code-review, security-review, implement-issue, interview | accepted at load; fidelity modes are tracked (task 8) |
| `project_memory=false` | code-review, security-review | ignored loudly today; tracked (task 8) |
| `stall_timeout` | code-review, security-review | supported: the stall watchdog (default 30 m, `0s` disables); a pending question parks it |
| `loop_restart_signature_limit`, `loop_restart` circuit breaker | fix-ci, implement-plan | supported: default 3, minimum 1, restart edges admit only `transient_infra`, counts survive restart and resume |
| `human.default_choice`, `review_target`, human gate `timeout` | interview | supported: expiry takes the default choice or Fabro's retry outcome; the review target is validated and shown |
| `goal_gate`, `retry_target` (graph and node), `max_visits`, `max_node_visits` | fix-ci, implement-plan | supported |
| `house` manager loop, `stack.child_workflow`, `manager.max_cycles` | implement-issue | supported: one child per manager attempt at one durable call site, Fabro's 45 s poll and `max_cycles` normalization, stop condition at each poll, cancellation and failure propagation (`crates/fabro/steps/tests/manager.rs`, `crates/fabro/acceptance/tests/workflow.rs`) |
| `hexagon` human gate, `question_type` (`yes_no`, `confirmation`, `multiple_choice`, `multi_select`, `freeform`), accelerator labels, `freeform=true` edge | interview | supported; a `multi_select` answer is `Answer::choices` (Fabro's `multi_selected` `option_keys`), the first key routes and every key and label is recorded |
| `tab` prompt node | interview | supported: `fabro/prompt`, one tool-free `lithos-llm` call with the output contract and repair turns; ACP is refused as Fabro refuses it (`crates/fabro/steps/tests/prompt.rs`, black box `a_prompt_node_makes_one_tool_free_model_call`) |
| `model`, `provider`, `reasoning_effort` on nodes | fix-ci, implement-plan | supported for `backend="api"` and prompt nodes; `provider="openrouter"` resolves through the catalog to OpenAI's chat completions protocol, which the OpenAI twin serves (black box `run_model_defaults_reach_openrouter_through_chat_completions`) |
| Conditions: `outcome=succeeded`, `outcome!=succeeded`, `context.K=V`, `&&` | all | supported (`!=` is in the grammar: `conditions.rs`, `holds("outcome!=failed", …)`) |
| `{{ inputs.* }}` in `goal`, `script`, prompts | code-review, security-review, fix-ci | supported; `[run.inputs]` defaults are read |
| `import` placeholders | none of the bundles; corpus and docs | supported at load with Fabro's prefixing, boundary rules, inherited defaults, class propagation, retry-target rewriting, nesting and cycle rejection (`lowering.rs`, black box `an_import_is_expanded_at_load_and_its_nodes_run_under_the_prefix`) |
| Output values above 100 KiB | code-review, security-review (large command output) | supported: the `OutputStore` capability with the local blob store; durable `blob://sha256/…` references, logical reads through `stdin_source` and prompts (`steps.rs`, `large_command_output_is_offloaded_and_reads_back_logically`) |

### `workflow.toml`

Fabro parses `workflow.toml` with the same schema as `settings.toml` and
`project.toml` (`lib/foundation/fabro-config/src/layers/settings.rs`).
Top-level keys are `_version`, `project`, `workflow`, `environments`, `run`,
`cli`, `server`, `llm`; anything else is a hard error. Nested tables use
`deny_unknown_fields`. Precedence, highest first: CLI overrides, `workflow.toml`,
`.fabro/project.toml`, `~/.fabro/settings.toml`, server defaults, built-in
defaults. `[run.inputs]` replaces wholesale across layers; `--input` wins per
key.

Petri acts on `[workflow] graph`, `[run.inputs]`, `[run] goal`,
`[run.model]`, `[run.execution]`, `[run.environment]` with `[environments.*]`,
and `[run.prepare]`. Every other section is diagnosed at load
(`crates/fabro/frontend/src/lower/workflow_toml.rs`) under the readiness
plan's item 1 rule: a platform-only option that has no effect warns
(`ignored.workflow_toml.<section>`, with why); a feature essential to the
requested work fails with a specific `unsupported.workflow_toml.<section>`
error before any node runs; a key Fabro's own parser refuses is
`unsupported.workflow_toml.key` with Fabro's rename hint. Nothing is dropped
silently. The whole file is validated before `[run.prepare]` runs. The
disposition column below is what `petri check` and `petri run` do today;
"tracked" names the task that implements the option, at which point its
diagnostic goes away.

| Section and options | Effect in Fabro | Petri disposition today |
|---|---|---|
| `_version` (`1`) | schema version | supported: any other value is `unsupported.workflow_toml.version`; the legacy `version` key is `unsupported.workflow_toml.key` |
| `[workflow]` `name`, `description`, `graph`, `metadata` | `graph` names the entry point | supported: `graph` selects the file; the rest is metadata |
| `[run]` `goal` (string or `{file}`), `working_dir`, `metadata` | goal text, local cwd | `goal`: supported, the run goal when the graph sets none (the graph attribute wins, as in Fabro); `working_dir`, `metadata`: warn (platform-only) |
| `[run.inputs]` | `{{ inputs.* }}` defaults | supported |
| `[run.model]` `provider`, `name`, `controls.reasoning_effort`, `controls.speed` | default model and request controls | supported: the defaults an LLM node gets below the graph's `default_model` / `default_provider`; `controls.speed` warns `ignored.workflow_toml.run.model.speed` (tracked, task 12) |
| `[run.model.fallbacks]` `"<model>" = [ "provider:model", ... ]` | model fallback chain | warn `ignored.workflow_toml.run.model.fallbacks` (tracked, task 12); used by code-review and security-review |
| `[run.prepare]` `steps[].script`/`command`/`env`, `timeout` (default 5m) | runs before the first node | supported: lowered as command nodes `run_prepare_N` between `start` and its successors, in the selected environment, with the step `env`, the section `timeout` and `on_failure="exit"`; exactly one of `script`/`command` per step (else `unsupported.workflow_toml.run.prepare`) |
| `[run.execution]` `mode` (`normal`, `dry_run`), `approval` (`prompt`, `auto`) | dry run and auto approve | supported as launch defaults (`Graph.params["fabro.launch"]`, read by the CLI); `--dry-run`, `--auto-approve`, `--interactive`, `--interview-script` win |
| `[run.environment]` `id`, `image`, `resources`, `network`, `lifecycle`, `labels`, `env` and `[environments.<id>]` `provider` (`local`, `docker`, `daytona`), `image.docker`, `image.dockerfile` (inline or `{path}`), `resources`, `network`, `lifecycle`, `labels`, `env` | sandbox selection | supported: `provider` selects the backend when `--backend` is absent (`local` host, `docker` Docker plugin, `daytona` Daytona plugin); `image.docker` is the scope's container image; `env` is the scope environment, with `{{ secrets.NAME }}` a `$secret` reference resolved at spawn from `PETRI_SECRET_NAME` (a missing secret fails the command `secret_unavailable`); `resources` size a Daytona runner. An unknown `id` or provider is an error. `cwd`, `network`, `lifecycle`, `labels`, `image.dockerfile` warn `ignored.workflow_toml.environments.<id>.<key>` (platform-only; the runner builds no image) |
| `[run.agent]` `fabro_tools` | run-management tools for agents | explicit exclusion: platform-only, warn `ignored.workflow_toml.run.agent.fabro_tools` when `true` |
| `[run.agent.mcps.<name>]` `id` or `type` (`http`, `stdio`, `sandbox`) with `url`, `headers`, `script`, `command`, `env`, `port`, `startup_timeout`, `tool_timeout`, `enabled` | MCP servers | error `unsupported.workflow_toml.run.agent.mcps` (tracked, task 13) |
| `[[run.hooks]]` `id`, `name`, `event`, `matcher`, `blocking`, `timeout`, `sandbox`, and one of `script`/`command`, `url`+`headers`+`tls`, `prompt`+`model`, `agent="enabled"`+`prompt`+`model`+`max_tool_rounds` | local hooks | error `unsupported.workflow_toml.run.hooks` (tracked, task 8): a configured hook is never skipped silently; `checkpoint_saved` stays an accepted difference (warn, do not run) once hooks land |
| `[run.checkpoint]` `exclude_globs`, `skip_git_hooks`, `commit_timeout` | Git checkpoints | explicit exclusion: platform Git; warn `ignored.workflow_toml.run.checkpoint` |
| `[run.clone]` `enabled`, `depth` | server clone depth | explicit exclusion: the standalone runner uses the given checkout; warn `ignored.workflow_toml.run.clone` |
| `[run.run_branch]`, `[run.meta_branch]` `enabled`, `push` | Git branches | explicit exclusion: platform Git; warn `ignored.workflow_toml.run.run_branch` / `run.meta_branch` |
| `[run.pull_request]` `enabled`, `draft`, `auto_merge`, `merge_strategy` | PR creation | explicit exclusion: platform publication; warn `ignored.workflow_toml.run.pull_request` |
| `[run.artifacts]` `include` | artifact upload globs | explicit exclusion for upload; local retention keeps the files; warn `ignored.workflow_toml.run.artifacts` |
| `[run.integrations.github]` `permissions`, `additional_repositories` | minted `GITHUB_TOKEN` | explicit exclusion: the run inherits the ambient token or none; warn `ignored.workflow_toml.run.integrations`. A bundle whose work needs the minted token (fix-ci) is blocked on a stand-in, task 17 |
| `[run.git.author]` `name`, `email` | checkpoint commit identity | explicit exclusion; warn `ignored.workflow_toml.run.git` |
| `[run.notifications.<name>]`, `[run.interviews]` `provider`, `slack.channel` | Slack | explicit exclusion; warn `ignored.workflow_toml.run.notifications` / `run.interviews` |
| `[run.scm]` | manifest metadata | explicit exclusion; warn `ignored.workflow_toml.run.scm` |
| `[project]`, `[cli.*]`, `[server.*]`, `[llm.*]` | inert in a workflow file | accepted but inert; warn `ignored.workflow_toml.<section>`; the legacy `[llm]` keys (`provider`, `model`, `temperature`, `max_tokens`, `fallbacks`, `fallback`) are `unsupported.workflow_toml.key` with the `[run.model]` hint, as in Fabro. `[environments.*]` without a `[run.environment]` that names one is inert |
| Rejected legacy top-level keys (`version`, `vars`, `setup`, `sandbox`, `hooks`, `mcp_servers`, ...) and unknown `[run]` keys | hard error with a rename hint | `unsupported.workflow_toml.key` with Fabro's rename hint |

Per-bundle `workflow.toml` use is recorded under `workflow_config.platform_only_sections`
in the lock file.

## Tracked departures to retire

1. **Parallel branch context** (task 6). Oracle case
   `static_fan_out_joins_all_branches`. Fabro keeps a branch's
   `context_updates` inside `parallel.results` and never merges them into the
   parent context. Petri merges them into `kv` and its branch results lack
   `context_updates`, so `parallel_values`-style helpers see nothing. The
   fixture records Fabro's result and Petri's current result side by side.

Retired by task 7: **failure promotion order**. Petri now follows Fabro's
executor: a failure an explicit route matches stays failed and takes that
route; an unmatched failure under `on_failure="succeed"` is promoted and
reports `succeeded`. Oracle cases `succeed_keeps_a_failure_an_explicit_edge_matches`,
`succeed_keeps_a_failure_a_preferred_label_matches` and
`succeed_promotes_a_failure_no_explicit_edge_matches` match the pinned
Fabro with no departure. The `partially_succeed` spelling stays as an
accepted difference (below).

## Accepted differences

These stay in the contract. They are tested as Petri differences, not as
reference expectations.

| Difference | Petri | Fabro |
|---|---|---|
| Firing cap | 500 firings per looped node; an explicit limit above 500 is refused at load | unlimited when visit limits are unset or zero |
| Invocation maximum | a hard 10,000 workflow invocations per run; cannot be raised or disabled | no counterpart |
| `outcome=success` | matched as `succeeded` with a warning until 2026-10-04, then refused | accepted, never matches |
| ACP tool hooks | best effort, with warnings about unenforceable hooks | ignored silently |
| `checkpoint_saved` hook | warning; the hook does not run | dispatched with no built-in behavior |
| Sensitive answers (`sensitive=true`, `$secret`) | a Petri extension | not defined |
| Skipped stages in the path | Petri records a `skipped` final outcome (oracle case `skipped_outcome_routes_like_success`) | no stage record |
| `on_failure="partially_succeed"` | a Petri extension: Fabro's `succeed` promotion order, but the promoted stage reports `partially_succeeded` (oracle case `partially_succeed_policy_classifies_before_routing`, warned `fabro.petri_extension`) | refused by the validator (`on_failure_valid`) |
| `[environments.*] image.dockerfile` | a warning; the scope runs on the selected backend's default runner image, or on `image.docker` when named | builds the image on the platform |
| Workflow secrets | `{{ secrets.NAME }}` resolves from `PETRI_SECRET_NAME` in the standalone runner; an embedding host supplies its own `SecretProvider` | the platform vault |
| Output references | `blob://sha256/<hex>` in a local store under `<run_dir>/blobs`, replaceable through the `OutputStore` capability; a structured value's reference carries `#json` | `blob://sha256/<hex>` in platform storage, materialized as `file://…/blobs/<hex>.json` for handlers |
| Prompt events | `StepEvent::Custom` with `kind = "fabro.prompt"` / `"fabro.prompt.completed"` (see `crates/fabro/FORMAT.md`) | `stage.prompt` / `prompt.completed` |

## Where things are

| Artefact | Path |
|---|---|
| Pin | `crates/fabro/corpus-pin.txt` |
| Bundle manifest | `crates/fabro/acceptance/bundles.lock.json` |
| Bundle fetcher | `scripts/corpus-fetch-fabro-bundles.sh` |
| Fabro fetcher | `scripts/corpus-fetch-fabro.sh` |
| Parity harness | `crates/fabro/oracle/harness/` (`oracle_harness.py`, `scripted_acp_agent.py`) |
| Fixture regeneration | `scripts/oracle-regenerate.sh` |
| Fake ACP agent | `crates/fabro/acceptance/testdata/fake_acp_agent.py` |
| Dependency direction tests | `crates/petri/lib/tests/fabro_dependencies.rs`, `crates/petri/cli/tests/standalone.rs` |
