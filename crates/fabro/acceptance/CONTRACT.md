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

## The differential matrix

Black box phase 5 (`crates/fabro/acceptance/DIFFERENTIAL.md`). Every
scenario under `crates/fabro/acceptance/scenarios/` runs through the shipped
`petri` binary and through the pinned Fabro binary
(`scripts/fabro-provision.sh`, never `PATH`'s `fabro`) with the same bundle,
inputs, provider twins and interview script, each engine in its own
workspace, server and twin namespace. The Fabro adapter
(`crates/petri/cli/tests/support/fabro/fabro_adapter.rs`) uses the same
public interfaces as the parity harness plus `fabro validate --json`,
`fabro dump` and `GET /api/v1/runs/{id}/state`, redirects Fabro's providers
through `[llm.providers.<id>] base_url` in its private `settings.toml`, and
answers gates through the questions API with `yes`, `no`, `selected`,
`multi_selected` and `text`.

Comparison rules (`support/fabro/compare.rs`): terminal status; the main
stage path in order; each fork's branch envelopes in dispatch order and the
causal order inside each branch, with exact counts; the workflow-owned
context key by key with named bookkeeping kept beside it; artifacts byte
for byte; interviews; provider requests (provider, model, effort, matched
twin scenario) with Fabro's platform requests listed apart; side-effect
counts. Only generated ids, the working directory and blob references are
normalized, through an explicit identity map. Each scenario asserts its
independent expectation on both engines first; a pinned-Fabro violation is
a recorded baseline defect, never permission for Petri to match it.

A difference is accepted only by a decision record in
`crates/fabro/acceptance/decisions/` (format in its `README.md`) that names
the difference kind and the scenario scope. The committed reference of a
scenario (`fabro-reference/reference.json`) is the baseline the live Fabro
run must reproduce; `PETRI_FABRO_REFERENCE_RECORD=1` is the only way to
refresh it. `crates/fabro/acceptance/tests/reference_version.rs` checks
every fixture, reference, capture, decision and evidence record against the
pin. `mise run test:fabro:differential` runs the matrix.

Cells captured live from the pinned binary: `parallel-results` (commands,
two fan-outs), `interview` (the required bundle, five gate kinds and the
summary prompt node), `edit-and-verify` (a native agent with a real shell
tool and a gate), `fallback-failover` (task 12's capture: a 503 after a
completed tool effect and the fall back to Anthropic), `skills-precedence`
(task 14's capture: the three skill directories, the reference prompt
section and `use_skill` tool, the repository's copy winning). Baseline defects of
the pinned Fabro found by the matrix: it repeats a completed tool effect
on failover (`fallback-repeated-tool-effect`).

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
| `stdin_source="context.parallel.results"` | code-review, security-review | supported: the fan-in publishes `parallel.results` (`id, index, item_label, status, context_updates` in branch order) and `parallel.branch_count`; `context_updates` is the branch's diff against the fork snapshot and never merges into the parent (`crates/fabro/acceptance/scenarios/parallel-results/CONTRACT.md`, black box `parallel_*` and `for_each_*`) |
| agent node (`prompt`, `@prompts/*.j2`, `{% include %}`) | all but fix-ci commands | supported |
| `output_schema="@schemas/*.json"`, `output_retries` | code-review, security-review | supported |
| `on_failure="route"`, `"exit"` (node and graph) | code-review, security-review | supported |
| `on_failure="succeed"` | code-review, security-review | supported with Fabro's promotion order: a failure an explicit route matches stays failed, an unmatched failure is promoted and reports `succeeded` (oracle cases `succeed_*`) |
| `max_retries`, `default_max_retries` | code-review, security-review, implement-plan | supported |
| `component` with `for_each`, `max_parallel`, `tripleoctagon` fan-in | code-review, security-review | supported: each branch is a child invocation from the fork snapshot, `max_parallel` is one slot per attempt (missing or invalid is 4, zero is 1, a branch in backoff holds none), static and `for_each` branches share the fan-in, empty lists join with no model call (`crates/fabro/steps/tests/parallel.rs`, `crates/core/execution/tests/admission.rs`) |
| `class` with `model_stylesheet` (including `{% set %}`, `{% if %}`, `inputs.*`) | code-review, security-review, implement-issue | supported |
| `default_fidelity`, `fidelity="truncate"`, `fidelity="summary:high"`, `thread_id`, `default_thread` | code-review, security-review, implement-issue, interview | supported: Fabro's preambles for every mode, resolution edge, node, graph, `compact`; threads resolved edge, node, graph, class, previous node, retained on the native backend at `full` (`crates/fabro/FORMAT.md`, "Fidelity and threads") |
| `project_memory=false` | code-review, security-review | supported: a prompt node reads no instruction files; agents read Fabro's per-profile files from the Git root to the working directory |
| Sub-agents (`spawn_agent`, `send_input`, `wait`, `close_agent` on native agents) | any bundle's agent may delegate | supported: every `backend="api"` agent has Pebble's sub-agent tools, as every API-backend agent has Fabro's; children share the scope, model and tool hooks, never the question tool; results, failure, cancellation, shutdown order and event identities match the reference (`crates/fabro/FORMAT.md`, "Native Pebble"); differences below |
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
| `[run.model]` `provider`, `name`, `controls.reasoning_effort`, `controls.speed` | default model and request controls | supported: the defaults an LLM node gets below the graph's `default_model` / `default_provider`; `controls.speed` is the default `speed` |
| `[run.model.fallbacks]` `"<model>" = [ "provider:model", ... ]` | model fallback chain | supported: chains keyed by the canonical requested model, resolved against the runner's catalog and available providers with Fabro's notices, reasoning effort mapped per target (`NoNearbyReasoningLevel`, `ChainEmpty`); a provider-local model error (the reference's `failover_eligible` mapping) moves a native agent or prompt stage to the next target, the conversation kept; see "Model fallback" in `crates/fabro/FORMAT.md`. Used by code-review and security-review |
| `[run.prepare]` `steps[].script`/`command`/`env`, `timeout` (default 5m) | runs before the first node | supported: lowered as command nodes `run_prepare_N` between `start` and its successors, in the selected environment, with the step `env`, the section `timeout` and `on_failure="exit"`; exactly one of `script`/`command` per step (else `unsupported.workflow_toml.run.prepare`) |
| `[run.execution]` `mode` (`normal`, `dry_run`), `approval` (`prompt`, `auto`) | dry run and auto approve | supported as launch defaults (`Graph.params["fabro.launch"]`, read by the CLI); `--dry-run`, `--auto-approve`, `--interactive`, `--interview-script` win |
| `[run.environment]` `id`, `image`, `resources`, `network`, `lifecycle`, `labels`, `env` and `[environments.<id>]` `provider` (`local`, `docker`, `daytona`), `image.docker`, `image.dockerfile` (inline or `{path}`), `resources`, `network`, `lifecycle`, `labels`, `env` | sandbox selection | supported: `provider` selects the backend when `--backend` is absent (`local` host, `docker` Docker plugin, `daytona` Daytona plugin); `image.docker` is the scope's container image; `env` is the scope environment, with `{{ secrets.NAME }}` a `$secret` reference resolved at spawn from `PETRI_SECRET_NAME` (a missing secret fails the command `secret_unavailable`); `resources` size a Daytona runner. An unknown `id` or provider is an error. `cwd`, `network`, `lifecycle`, `labels`, `image.dockerfile` warn `ignored.workflow_toml.environments.<id>.<key>` (platform-only; the runner builds no image) |
| `[run.agent]` `fabro_tools` | run-management tools for agents | explicit exclusion: platform-only, warn `ignored.workflow_toml.run.agent.fabro_tools` when `true` |
| `[run.agent]` `skills` | refused: `[run.agent]` denies unknown keys; skills come from `$FABRO_HOME/skills`, `<root>/.fabro/skills`, `<root>/skills` (`fabro-agent/src/skills.rs`) | a Petri extension (accepted difference below): a list of extra directories searched after Fabro's three, warned `fabro.petri_extension`; any other shape is `unsupported.workflow_toml.run.agent.skills`. Fabro's three directories, their order and precedence are implemented for native agents (`crates/fabro/FORMAT.md`, "Skills"; fixtures `testdata/skills`) |
| `[run.agent]` `compaction` | refused: `[run.agent]` denies unknown keys; compaction is always on with hardcoded values (`fabro-agent`'s `SessionOptions`: 80 percent trigger, six preserved turns) | refused `unsupported.workflow_toml.key`, with a message naming the values in force; those values are lowered onto every agent node and run through Pebble's automatic compaction (`crates/fabro/FORMAT.md`, "Compaction") |
| `[run.agent.mcps.<name>]` `type` (`http`, `stdio`, `sandbox`) with `url`, `headers`, `protocol`, `script`, `command`, `env`, `port`, `startup_timeout`, `tool_timeout`, `enabled`; or `id` | MCP servers for agent sessions | supported: every inline field with Fabro's rules and defaults, merged across the settings, project and workflow layers by name (`crates/fabro/FORMAT.md`, "MCP servers"); stdio servers on Petri's host, http servers from the host, sandbox servers on a host-backed scope only; tools registered with Pebble under `mcp__<server>__<tool>` and run through the normal tool path, hooks included; explicit exclusions: `id` (a server-managed catalog reference, `unsupported.workflow_toml.run.agent.mcps.reference`), `protocol = "sse"` (`unsupported.workflow_toml.run.agent.mcps.protocol`), a secret token outside a whole `env`/`headers` value (`unsupported.workflow_toml.run.agent.mcps.secret`); `{{ env.* }}` is an error as in Fabro |
| `[[run.hooks]]` `id`, `name`, `event`, `matcher`, `blocking`, `timeout`, `sandbox`, and one of `script`/`command`, `url`+`headers`+`tls`, `prompt`+`model`, `agent="enabled"`+`prompt`+`model`+`max_tool_rounds` | local hooks | supported: every field, transport and event, merged with `.fabro/project.toml` and the host's settings layer by `id`, run by the local hook service with Fabro's matching, decisions, placement and timeouts (`crates/fabro/FORMAT.md`, "Hooks"); a layer that cannot be read is an error, so a configured hook is never skipped silently; `checkpoint_saved` warns and does not run (accepted difference below); `stage_retrying` is dispatched (the reference never fires it) |
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

Found by the differential matrix (task 18) and recorded as a decision record
with its retirement condition:

- **The interview bundle needs a `[run.model]` default**
  (`decisions/interview-run-model-migration.toml`). Fabro takes the run's
  model from the launch; the standalone runner has no launch-level model
  default, so the matrix runs a recorded migration of the bundle's
  `workflow.toml`. Retire when `petri run` gains a launch-level default.

Resolved by the matrix (Petri fixed, no departure): a `yes_no` or
`confirmation` gate records `yes`/`no` under `human.gate.<node>.answer` as
Fabro does, and a freeform answer sets `human.gate.label`.

Retired with the sandbox-driver re-pin (`a225832`): **command output gained
a final newline**. The plugin's exec path was byte-exact all along; the
newline came from Petri's line pump, which now records whether the process
terminated each line (`LogLine::terminated`), and the command step joins the
lines accordingly, so `command.output` is the script's output byte for byte
(`printf 'a\nb'` yields `a\nb`). The `command-output-trailing-newline`
record is deleted; the differential cells pass with `petri == fabro`
exactly, and a recurrence is reported as an unresolved
`value.trailing_newline` difference.

Retired by task 6: **parallel branch context**. Each branch now runs as a
child invocation from the fork snapshot, the fan-in publishes
`parallel.results` with every branch's `context_updates`, and nothing merges
into the parent context. Oracle case `static_fan_out_joins_all_branches`
records an empty context on both sides; its remaining difference is the path
(Petri lists the branch stages, Fabro records branch events), listed below.

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
reference expectations. Each row is one decision record under
`crates/fabro/acceptance/decisions/`, which is the index the differential
comparison loads; two records found by the matrix have no row here:
`fabro-run-title-call` (Fabro's run-title request on the provider's small
default model before the first stage) and `fallback-repeated-tool-effect`
(a baseline defect, not an accepted Petri behaviour).

| Difference | Petri | Fabro |
|---|---|---|
| Firing cap | 500 firings per looped node; an explicit limit above 500 is refused at load | unlimited when visit limits are unset or zero |
| Invocation maximum | a hard 10,000 workflow invocations per run; cannot be raised or disabled | no counterpart |
| `outcome=success` | matched as `succeeded` with a warning until 2026-10-04, then refused | accepted, never matches |
| ACP tool hooks | best effort at `session/request_permission`, with a `fabro.hook.warning` naming the backend, hook, event and missing boundary for each unenforceable hook | ignored silently |
| `stage_retrying` hook | dispatched before each retry attempt; its decision is ignored | declared, never dispatched |
| Run-level hook reports | `run_complete`, `run_failed` and `sandbox_cleanup` reports are logged, not recorded in the event log (they belong to no firing) | platform events |
| Terminal echo | a stage's echoed output is bounded at 64 KiB (one marker names the log file, which keeps everything); a branch's lines carry its invocation (`[invocation-N/node#firing]`) | the platform's log viewer |
| `[run.agent]` keys | `subagents`, `compaction` and any key other than `fabro_tools`, `mcps` and `skills` are refused (`unsupported.workflow_toml.key`): the pinned Fabro's `[run.agent]` denies unknown keys and has no setting for sub-agents or compaction; sub-agents are always on for native agents, so the `subagents` refusal says so | refused by the parser |
| Context compaction | always on with Fabro's values (80 percent trigger, six preserved turns); Pebble's automatic compaction summarizes and replaces the history, and Petri never rewrites it (`crates/fabro/FORMAT.md`, "Compaction") | identical values, self-contained in `fabro-agent` |
| Compaction summary usage | the pinned Pebble leaves the summary call out of a prompt's usage, so Petri reports it separately: a `fabro.compaction` event and the `pebble.compaction_*` metrics carry the summary usage read from the `Compaction` turn in Pebble's history. Pebble `861d9bc` folds it into the prompt's usage; recommended re-pin below | the summary usage is dropped entirely (`compact_context` discards `response.usage`) |
| `[run.agent] skills` | a Petri extension: extra skill directories searched after Fabro's three, warned `fabro.petri_extension` | refused by the parser |
| Skill files that do not parse | reported: a `fabro.skills.warning` event and a stderr line per malformed or unreadable `SKILL.md`, and per workflow-named directory that does not exist; the file is still skipped, as Pebble skips it | skipped silently |
| A prompt naming a missing skill (`/name`) | the node fails with class `skill_missing` | the stage fails with an `InvalidState` error |
| Sub-agent nesting and concurrency | a child may delegate again; one tree holds at most 4 sessions open at once (the root included; a finished child holds its slot until closed), and a spawn over the bound is the tool's answer (`Cannot spawn another agent`) | one level only (a child has no sub-agent tools; `max_subagent_depth = 1`); no bound on concurrent children |
| Sub-agent project memory | a child reads no `AGENTS.md`/profile documents and no skill directories (Pebble's rule: "a task, not a project briefing"; library contract item in `.ai/reviews/fabro-unified/task15-subagents.md`) | a child re-discovers the project documents and skills from the shared sandbox |
| Sub-agent MCP tools | a child inherits the workflow's `[run.agent.mcps]` tools (task 13 registers them `allow_in_subagents`) and calls them through the parent's one connection, under the parent's hooks and attribution; readiness items 9b and 9d ask for inherited tools | a child session is built without `mcp_servers`, so it has no MCP tools |
| Sub-agent usage | the stage's `pebble.usage` is the parent session's own; the children's usage is the `pebble.subagents` metric and the `agent_activity` events | child usage merges into the stage's usage total through the event stream |
| `checkpoint_saved` hook | warning; the hook does not run | dispatched with no built-in behavior |
| Sensitive answers (`sensitive=true`, `$secret`) | a Petri extension | not defined |
| Skipped stages in the path | Petri records a `skipped` final outcome (oracle case `skipped_outcome_routes_like_success`) | no stage record |
| Parallel branches in the path | each branch's stage has a final record in its child invocation, so the path lists the branches in branch order between the fork and the join (oracle case `static_fan_out_joins_all_branches`) | branches are `parallel` events, not stages; the path skips them |
| `context_updates` of a branch | the diff against the fork snapshot: a key a branch writes back with the value it already had is not reported; a failure with an empty `failure_class` is dropped | every key the branch wrote |
| `for_each` over an empty list | the template fires once with the placeholder item `petri.parallel.empty`; the fan-in strips it and joins zero results with no model call | zero branches |
| `on_failure="partially_succeed"` | a Petri extension: Fabro's `succeed` promotion order, but the promoted stage reports `partially_succeeded` (oracle case `partially_succeed_policy_classifies_before_routing`, warned `fabro.petri_extension`) | refused by the validator (`on_failure_valid`) |
| `[environments.*] image.dockerfile` | a warning; the scope runs on the selected backend's default runner image, or on `image.docker` when named | builds the image on the platform |
| Workflow secrets | `{{ secrets.NAME }}` resolves from `PETRI_SECRET_NAME` in the standalone runner; an embedding host supplies its own `SecretProvider` | the platform vault |
| Output references | `blob://sha256/<hex>` in a local store under `<run_dir>/blobs`, replaceable through the `OutputStore` capability; a structured value's reference carries `#json` | `blob://sha256/<hex>` in platform storage, materialized as `file://…/blobs/<hex>.json` for handlers |
| Prompt events | `StepEvent::Custom` with `kind = "fabro.prompt"` / `"fabro.prompt.completed"` (see `crates/fabro/FORMAT.md`) | `stage.prompt` / `prompt.completed` |
| MCP events | `StepEvent::Custom` with `kind = "fabro.mcp.server"` (`starting`, `ready`, `failed`, `disconnected`, `stopped`) and `"fabro.mcp.tool"` (one per proxied call, with `status` and `duration_ms`); the failure line `mcp server \`<name>\` failed to start: ...` on the node's stderr | `agent.mcp.ready` / `agent.mcp.failed`; tool activity only through the generic tool events |
| MCP catalog references | `[run.agent.mcps.<name>] id = "..."` is refused at load: the standalone runner has no server-managed catalog | resolved from the server's catalog |
| MCP legacy SSE | `protocol = "sse"` is refused at load; only streamable HTTP is spoken (the pinned `rmcp` has no legacy SSE client; Fabro carries its own) | supported |
| MCP secrets | `{{ secrets.NAME }}` resolves as a whole `env` or `headers` value only, from `PETRI_SECRET_NAME` (or the host's provider) at launch; a token inside a command, script or URL is refused at load | resolved anywhere in the transport strings at the run boundary from the vault |
| MCP `sandbox` transport | launched through the scope's execution environment and reached at `http://localhost:<port>`; a container or remote scope fails the server with a named reason (the plugin protocol exposes no port or preview URL) | a Daytona preview URL; local sandboxes fall back to localhost |
| MCP stdio working directory | the scope's workspace when the scope shares the host filesystem, else Petri's own directory | the run worker's directory |
| Model fallback: session handoff | the failed session's record resumes on the next route (`ResumeMode::UseModel`, same session id); a turn whose tool effects already ran is continued with an agent-sourced continuation message, never repeated | the session is discarded and a new one runs the original prompt from scratch on the next route, repeating any tool effect |
| Model fallback: provider-only candidates | a bare provider in a chain resolves to the same model id on that provider when its catalog lists it, else `NoCompatibleModel` | picks the provider's closest model by feature profile and price |
| Model fallback: unknown selectors | a selector no catalog row names is skipped with a notice unless the provider allows passthrough models | passed through for the provider to validate |
| Model fallback: configuration errors | a bad chain (provider-named or qualified key, two keys for one model, unknown key or provider) fails the first LLM stage with class `bad_config` | fails run start |
| Model fallback: ACP agents | no plan; the ACP command owns its model | the same |
| Model fallback: events | `StepEvent::Custom` kinds `fabro.fallback.{plan,route,usage,failover,stop}` and the once-per-run stderr notices | `agent.failover` events and run notices |
| Model fallback: recovery | a resumed node starts a new plan at position 0 on the primary; a request in flight at the crash may be sent again | sessions persist server-side |
| Client retries on the agent event stream | `lithos-llm`'s same-route retries are not on the Pebble event stream: the pinned Pebble does not export `RetryEventObserver` | the reference's client reports its retries |
| MCP server lifetime | one set of servers per agent node session, started before the agent and stopped after it (a retained thread's next node starts its own); a resumed run starts them again | one set per agent session; the same |

## Advanced agent milestones (item 9, milestones C1 to C5)

The overall item 9 gate: all five advanced-agent stages pass their own
acceptance gates. Status at task 16's finishing time (2026-09-07). Task 16
owns C5 and this checklist; the other four stages ran in parallel, so a stage
whose branch had not merged onto `swarm/integration` when task 16 finished is
marked "not verified" with the reason, not "passed".

| Stage | Acceptance gate | Branch / commit | Status |
|---|---|---|---|
| C1 model fallback and failover (item 9a, task 12) | scripted provider failures exercise selection order, session handling, terminal outcome, and complete usage/events | `swarm/task12-fallback`, commits `240e0c9`..`f0ce08e` and after (evidence `task12-fallback.md`) | passed on the branch with integration merged (tasks 13 to 16 in): selection order and the reference's notices (`fallback::tests`, 9 unit tests); through the binary with both twins injecting failures (`fabro_fallback_blackbox`, 15 cases: primary, qualifying and non-qualifying, a third provider, exhaustion, a tool effect kept across the handoff, cancellation, refusal, timeout, client retries, a workflow retry, effort mapping, repair turns, a retained thread, a prompt node); the outcome and per-route accounting rebuilt from public events (`fallback_events`, 3); the client's own retries and budget (`llm_client`, 2). Reference sequences derived from the pinned source, not captured from the binary |
| C2 MCP execution (item 9b, task 13) | a configured local MCP server's tool effect, hooks, output, events, cancellation and shutdown are verified | `swarm/task13-mcp`, commits `b67d3ee`..`72be261` (evidence `task13-mcp.md`) | passed: merged; the MCP suites and the scripted stdio server (`fabro_mcp_blackbox`, `testdata`) pass in the gate (1192) |
| C3 skills (item 9c, task 14) | versioned fixtures verify skill discovery, precedence, loading, prompt/tool behavior and events without Fabro | `swarm/task14-skills`, commits `c27fc3e`, `bdd5c5a` (evidence `task14-skills.md`) | passed: merged; the skills suites and fixtures (`testdata/skills`) pass in the gate; skill context is loaded into the system prompt, outside the agent history, so compaction cannot remove it (verified below) |
| C4 sub-agents (item 9d, task 15) | a parent delegates real work; results, ownership, cancellation, hooks and child identities match the reference | `swarm/task15-subagents`, commits `8861076`..`303f682`, `e8d20e1`, `269daca`, `ad2d472`, the merges `bd14208`, `3cbc1bc` and the task 16 merge (evidence `task15-subagents.md`) | passed: every native agent has Pebble's sub-agent tools (the pinned Fabro has no setting either; `[run.agent] subagents` stays refused because Fabro's parser refuses it); a parent delegates a workspace change to a child, hooks block inside a child, a child's failure is the parent's tool result, concurrent children and a grandchild, an interrupt closes the child, a retained thread keeps a child's result, resume restarts the stage, children never count against the invocation ceiling, accounting reconstructs from the public events. In-process `petri-fabro-steps::subagents` (12, two ignored contract tests for child memory and skills), black box `fabro_subagents_blackbox` (8, one with an inherited MCP tool). Accepted differences above: nesting under the open-session bound, no project memory or skills in a child, usage beside the parent's, inherited MCP tools |
| C5 context compaction (item 9e, task 16) | controlled histories trigger compaction and preserve required conversation/tool state, later thread use, usage, and events | `swarm/task16-compaction`, this branch | passed: the trigger below, at and above the 80 percent threshold; continuation, thread reuse, tool pairing across the boundary, summary failure, cancellation, resume fallback; public events and usage. In-process `petri-fabro-steps::compaction` (7), black box `fabro_compaction_blackbox` (4) |

Skill context across compaction (item 9e's cross-feature check, C3 landed):
skills reach a native session through Pebble's `with_skill_dirs`, which folds
the discovered skills into the session's system prompt and registers the
`use_skill`/`Skill` tool. The system prompt and the tool registry live on the
session outside `History`; compaction only ever replaces turns inside
`History` (`pebble-coding-agent` `compact_from`). So a compacted session keeps
its skills and its skill tool.

Sub-agents (C4) landed: the supervisor lives on the session outside
`History`, so a parent that compacts keeps it; verified end to end by
`petri-fabro-steps::subagents::a_parent_still_delegates_after_its_own_compaction`
(the turn after the parent's compaction spawns a child and waits for it, and
the later request carries the summary, not the discarded output). A child
inherits the parent's compaction settings and policy through Pebble's
`ChildDeps`: `a_child_compacts_under_the_inherited_settings_and_its_events_name_the_child`
shows a child crossing the trigger, its own summary call, Pebble's
`CompactionStarted`/`CompactionCompleted` on the shared stream under the
child's session naming the parent, and the stage's `pebble.subagents.sessions`
entry counting the compaction. A child's compaction produces no
`fabro.compaction` event and no `pebble.compaction_usage`: that event is read
from the parent agent's own history, and Pebble's `CompactionCompleted`
carries no usage, so a child's summary usage is unreported at the pin (the
recommended re-pin below folds it into the child's own usage, which the
ledger sums).

MCP (C2) landed: MCP tools are registered on the session with Pebble's
`tools(mcp.tools())` (`pebble.rs`), so they live in the session's tool
registry, outside `History`; compaction replaces only `History`, so a compacted
session keeps its MCP tools. Verified by construction against the reference,
the same way skills are; an end-to-end MCP-tool-after-compaction black box is a
follow-up for the combined milestone D coverage (item 10).

## Recommended library re-pin

Pebble `861d9bc` (one commit past the pin `a2fcdda`) is
"Test coding across compaction and account for summary usage". Its only
library change folds the automatic-compaction summary call's usage and cost
into the prompt's `PromptReport.usage`/`cost_usd_micros`, which the pin drops.
Recommended re-pin for the coordinator's batched library pass after wave 3.
With it, Petri can drop the `fabro.compaction` usage read and the
`pebble.compaction_*` metrics and read the summary usage from the prompt
report like every other call. The failing-at-the-pin contract test is
`petri-fabro-steps::compaction the_trigger_is_strictly_above_eighty_percent_of_the_window`'s
usage assertion, which today asserts the prompt usage excludes the summary
(`pebble.usage.input == 4 * 10 + total - 5`); at `861d9bc` it would include it.

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
| Fabro provisioning | `scripts/fabro-provision.sh` (cache `crates/fabro/corpus/fabro-target/`) |
| Differential matrix | `crates/petri/cli/tests/fabro_differential.rs`, `tests/support/fabro/{fabro_adapter,compare,evidence}.rs`, `crates/fabro/acceptance/DIFFERENTIAL.md` |
| Decision records | `crates/fabro/acceptance/decisions/` |
| Scenario references | `crates/fabro/acceptance/scenarios/<name>/fabro-reference/reference.json` |
| Reference-version checks | `crates/fabro/acceptance/tests/reference_version.rs` |
