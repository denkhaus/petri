# Corpus run sweep

The run-time counterpart of REPORT.md: every in-scope workflow that lowers, run end to end with `run:` scripts stubbed to `true` and `uses:` steps real. The outcome measures the runtime tier — checkout, action staging and execution, artifact and cache backends, cross-job flow — not the corpus projects' own builds.

Sweep configuration: host scopes rewritten to the pinned runner images `ghcr.io/lithoscomputer/ubuntu-24.04:slim-66c538cd3ef8` (22.04/26.04 variants by label), matrices capped to their first leg, each workflow capped at 300s wall clock, parallelism 6. Identity: `github.sha` is the repo's pinned corpus commit (`corpus-pins.txt`) — the sweep's analog of `default_params` reading HEAD — so `checkout` fetches real state; a dummy `GITHUB_TOKEN` keeps the sweep token-less by policy.

249 workflows in scope; 240 lower and were run.

| Result (of the 240 run) | Count | Share |
|---|---|---|
| passed | 46 | 19% |
| **failed on a runtime-tier gap** | 41 | 17% |
| expected failure (server-coupled) | 153 | 64% |
| **timed out** | 0 | 0% |

## First failures — gaps, ranked

| First failing step · class | Workflows |
|---|---|
| `actions/stale · exit_status:1` | 7 |
| `actions/setup-node · exit_status:1` | 5 |
| `noop · env_acquire` | 5 |
| `actions/github-script · exit_status:1` | 4 |
| `noop` | 4 |
| `actions/setup-python · exit_status:1` | 2 |
| `astral-sh/setup-uv · exit_status:1` | 2 |
| `dessant/lock-threads · exit_status:1` | 2 |
| `github/gh-aw-actions/setup · exit_status:1` | 2 |
| `release-drafter/release-drafter · exit_status:1` | 2 |
| `Dockerfile · action_image` | 1 |
| `actions/cache/restore · bad_config` | 1 |
| `dorny/paths-filter · exit_status:1` | 1 |
| `github/issue-labeler · exit_status:1` | 1 |
| `mheap/github-action-required-labels · exit_status:1` | 1 |
| `run: · env_acquire` | 1 |

## Expected failures — server-coupled, ranked

First failures no token-less local run can fix: OIDC, GitHub App and repository secrets, git credentials for the real checkout action, third-party SaaS backends, cross-run artifact reads. Kept out of the gap ranking so it never drowns in server-bound noise.

| First failing step · why | Workflows |
|---|---|
| `actions/checkout · needs a git credential (the sweep is token-less)` | 130 |
| `actions/checkout · needs a repository secret` | 8 |
| `actions/create-github-app-token · needs a repository secret` | 5 |
| `actions/github-script · needs a repository secret` | 2 |
| `open-security-tools/ost-simple-sts · needs a repository secret` | 2 |
| `actions/download-artifact · cross-run artifact download (REST API)` | 1 |
| `actions/stale · needs a repository secret` | 1 |
| `dessant/lock-threads · needs a repository secret` | 1 |
| `nodejs/node-pr-labeler · needs a repository secret` | 1 |
| `tsickert/discord-webhook · needs a repository secret` | 1 |
| `withgraphite/graphite-ci-action · needs a repository secret` | 1 |

## Every workflow

| Repository | Workflow | Result | First failure |
|---|---|---|---|
| actions/checkout | `check-dist.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| actions/checkout | `codeql-analysis.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| actions/checkout | `licensed.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| actions/checkout | `publish-immutable-actions.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| actions/checkout | `update-main-version.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp http\.https\:\/\/github\.com\/\.extraheader` _(needs a git credential (the sweep is token-less))_ |
| actions/checkout | `update-test-ubuntu-git.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `build-docker.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `build-wasm.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `daily_fuzz.yaml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `memory_report.yaml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `notify-dependents.yml` | expected failure | `actions/github-script` — no secret named `RUFF_PRE_COMMIT_PAT` _(needs a repository secret)_ |
| astral-sh/ruff | `publish-crates.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `publish-docs.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `publish-mirror.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/ruff | `publish-playground.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `publish-pypi.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/ruff | `publish-ty-playground.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `publish-versions.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/ruff | `publish-wasm.yml` | expected failure | `actions/download-artifact` — step exited with status 1 · `Error: Unable to download artifact(s): Unable to get the ACTIONS_RUNTIME_TOKEN env variable` _(cross-run artifact download (REST API))_ |
| astral-sh/ruff | `ty-ecosystem-analyzer.yaml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `ty-ecosystem-report.yaml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `typing_conformance.yaml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `bench.yml` | not lowered | `runs_on.unknown` |
| astral-sh/uv | `build-docker.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `check-docs.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `check-fmt.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `check-generated-files.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `check-lock.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `check-publish.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `check-release.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `check-zizmor.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `diagnose-workflow-failure.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `fix-bug.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `issue-triage.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `plan.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `promote-pull-request.yml` | pass | — |
| astral-sh/uv | `publish-crates.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `publish-docs.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `publish-mirror.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/uv | `publish-pypi.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `publish-versions.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/uv | `pull-request-conflicts.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `pull-request-labels.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `pull-request-security-review.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `rebase-conflicted-pull-request.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `release-prepare.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `reproduce-bug.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `sync-python-releases.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `sync-uv-dev.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `sync-uv-security.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `test-ecosystem.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `update-issue-context.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `update-pull-request-parent.yml` | pass | — |
| cli/cli | `agentics-maintenance.yml` | **fail** | `actions/cache/restore` — step config is invalid: unsafe action path `../dist/restore-only/index.js`: every path component must be a normal name |
| cli/cli | `bump-go.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| cli/cli | `codeql.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| cli/cli | `copilot-setup-steps.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| cli/cli | `dependabot-triage.lock.yml` | **fail** | `github/gh-aw-actions/setup` — step exited with status 1 · `Failed to run setup.sh: spawnSync /workspace/.ci/github/actions/github/gh-aw-actions/423b3dc04bbf1b1797194a4a75aa5cf5d0d4f5b3/setup/setup.sh EACCES` |
| cli/cli | `govulncheck.yml` | expected failure | `actions/checkout` — step exited with status 1 · `Removing includeIf entries pointing to credentials config files` _(needs a git credential (the sweep is token-less))_ |
| cli/cli | `issue-triage.lock.yml` | **fail** | `github/gh-aw-actions/setup` — step exited with status 1 · `Failed to run setup.sh: spawnSync /workspace/.ci/github/actions/github/gh-aw-actions/423b3dc04bbf1b1797194a4a75aa5cf5d0d4f5b3/setup/setup.sh EACCES` |
| cli/cli | `lint.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| cli/cli | `triage-issues.yml` | pass | — |
| cli/cli | `triage-pull-requests.yml` | pass | — |
| cli/cli | `triage-scheduled-tasks.yml` | pass | — |
| denoland/deno | `cargo_publish.generated.yml` | expected failure | `actions/checkout` — no secret named `DENOBOT_PAT` _(needs a repository secret)_ |
| denoland/deno | `create_prerelease_tag.generated.yml` | expected failure | `actions/checkout` — no secret named `DENOBOT_PAT` _(needs a repository secret)_ |
| denoland/deno | `post_publish.generated.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| denoland/deno | `pr.generated.yml` | pass | — |
| denoland/deno | `start_release.generated.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| denoland/deno | `version_bump.generated.yml` | expected failure | `actions/checkout` — no secret named `DENOBOT_PAT` _(needs a repository secret)_ |
| django/django | `benchmark.yml` | pass | — |
| django/django | `check-migrations.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| django/django | `check_commit_messages.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| django/django | `check_pr_quality.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| django/django | `coverage_comment.yml` | pass | — |
| django/django | `coverage_tests.yml` | pass | — |
| django/django | `docs.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| django/django | `labels.yml` | **fail** | `actions/github-script` — step exited with status 1 · `    at Module._load (node:internal/modules/cjs/loader:1396:12)` |
| django/django | `linters.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| django/django | `new_contributor_pr.yml` | pass | — |
| django/django | `playwright.yml` | pass | — |
| django/django | `postgis.yml` | pass | — |
| django/django | `python_matrix.yml` | pass | — |
| django/django | `schedules.yml` | expected failure | `actions/github-script` — no secret named `SCHEDULE_WORKFLOW_TOKEN` _(needs a repository secret)_ |
| django/django | `screenshots.yml` | pass | — |
| facebook/react | `compiler_discord_notify.yml` | pass | — |
| facebook/react | `compiler_playground.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| facebook/react | `compiler_prereleases.yml` | expected failure | `actions/checkout` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_prereleases_manual.yml` | expected failure | `actions/checkout` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_prereleases_nightly.yml` | expected failure | `actions/checkout` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_rust.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| facebook/react | `compiler_typescript.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| facebook/react | `devtools_discord_notify.yml` | pass | — |
| facebook/react | `devtools_regression_tests.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| facebook/react | `runtime_build_and_test.yml` | not lowered | `step.background` |
| facebook/react | `runtime_commit_artifacts.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| facebook/react | `runtime_discord_notify.yml` | pass | — |
| facebook/react | `runtime_eslint_plugin_e2e.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| facebook/react | `runtime_fuzz_tests.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git -c protocol.version=2 fetch --no-tags --prune --no-recurse-submodules --depth=1 origin +a1124489a5e8f81e16ac957699a60038b965f502:refs/remotes/origin/main` _(needs a git credential (the sweep is token-less))_ |
| facebook/react | `runtime_release_from_ci.yml` | expected failure | `tsickert/discord-webhook` — no secret named `DISCORD_WEBHOOK_URL` _(needs a repository secret)_ |
| facebook/react | `runtime_sizebot_comment.yml` | pass | — |
| facebook/react | `shared_check_maintainer.yml` | **fail** | `actions/github-script` — step exited with status 1 · `}` |
| facebook/react | `shared_cleanup_merged_branch_caches.yml` | pass | — |
| facebook/react | `shared_cleanup_stale_branch_caches.yml` | pass | — |
| facebook/react | `shared_close_direct_sync_branch_prs.yml` | **fail** | `actions/github-script` — step exited with status 1 · `Error: Unhandled error: SyntaxError: Unexpected token ';'` |
| facebook/react | `shared_label_core_team_prs.yml` | pass | — |
| facebook/react | `shared_lint.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| facebook/react | `shared_stale.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials` |
| hashicorp/terraform | `backport.yml` | **fail** | `run:` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| hashicorp/terraform | `changelog-validation.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| hashicorp/terraform | `checks.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp http\.https\:\/\/github\.com\/\.extraheader` _(needs a git credential (the sweep is token-less))_ |
| hashicorp/terraform | `enforce-changelog.yml` | **fail** | `dorny/paths-filter` — step exited with status 1 · `Error: The process 'git rev-parse --abbrev-ref HEAD' failed with exit code 128` |
| hashicorp/terraform | `equivalence-test-diff.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| hashicorp/terraform | `equivalence-test-manual-update.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| hashicorp/terraform | `equivalence-test-update.yml` | pass | — |
| hashicorp/terraform | `issue-comment-created.yml` | pass | — |
| hashicorp/terraform | `lock.yml` | **fail** | `dessant/lock-threads` — step exited with status 1 · `Error: Bad credentials - https://docs.github.com/rest` |
| nodejs/node | `auto-start-ci.yml` | pass | — |
| nodejs/node | `build-tarball.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `close-stalled.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| nodejs/node | `codeql.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `comment-labeled.yml` | pass | — |
| nodejs/node | `commit-lint.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `commit-queue.yml` | pass | — |
| nodejs/node | `coverage-linux-without-intl.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `coverage-linux.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `create-release-proposal.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `daily-wpt-fyi.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Version ~3.14.0-0 was not found in the local cache` |
| nodejs/node | `daily.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `doc.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `find-inactive-collaborators.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `find-inactive-tsc.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `label-flaky-test-issue.yml` | pass | — |
| nodejs/node | `label-pr.yml` | expected failure | `nodejs/node-pr-labeler` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `license-builder.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `lint-release-proposal.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `linters.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp http\.https\:\/\/github\.com\/\.extraheader` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `major-release.yml` | pass | — |
| nodejs/node | `nix-changes-comment.yml` | pass | — |
| nodejs/node | `notify-on-push.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: Bad credentials` |
| nodejs/node | `notify-on-review-wanted.yml` | pass | — |
| nodejs/node | `post-release.yml` | pass | — |
| nodejs/node | `scorecard.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'core\.sshCommand' && git config --local --unset-all 'core.sshCommand' \|\| :"` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| nodejs/node | `test-internet.yml` | pass | — |
| nodejs/node | `test-linux-quic.yml` | not lowered | `action.local_missing` |
| nodejs/node | `test-linux.yml` | not lowered | `action.local_missing` |
| nodejs/node | `test-shared.yml` | not lowered | `runs_on.expression` |
| nodejs/node | `timezone-update.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `tools.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `update-openssl.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `update-v8.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `update-wpt.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| ohmyzsh/ohmyzsh | `dependencies.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| ohmyzsh/ohmyzsh | `main.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| ohmyzsh/ohmyzsh | `project.yml` | expected failure | `actions/create-github-app-token` — no secret named `OHMYZSH_CLIENT_ID` _(needs a repository secret)_ |
| ohmyzsh/ohmyzsh | `scorecard.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `benchmark-remote.yml` | not lowered | `runs_on.unknown` |
| pola-rs/polars | `benchmark.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `changes-dsl-labeler.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `clear-caches.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `docs-python.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `docs-rust.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `issue-labeler.yml` | **fail** | `github/issue-labeler` — step exited with status 1 · `Error: HttpError: Bad credentials` |
| pola-rs/polars | `lint-global.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `lint-python.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `lint-rust.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `pr-labeler.yml` | **fail** | `release-drafter/release-drafter` — step exited with status 1 · `Error: Invalid config file` |
| pola-rs/polars | `release-drafter.yml` | **fail** | `release-drafter/release-drafter` — step exited with status 1 · `Error: Invalid config file` |
| pola-rs/polars | `release-rust.yml` | pass | — |
| pola-rs/polars | `test-bytecode-parser.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `test-pyodide.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| prometheus/prometheus | `approve-workflows.yml` | pass | — |
| prometheus/prometheus | `automerge-dependabot.yml` | pass | — |
| prometheus/prometheus | `buf-lint.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| prometheus/prometheus | `buf.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| prometheus/prometheus | `check_release_notes.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| prometheus/prometheus | `codeql-analysis.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| prometheus/prometheus | `container_description.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| prometheus/prometheus | `fuzzing.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| prometheus/prometheus | `govulncheck.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| prometheus/prometheus | `lock.yml` | expected failure | `dessant/lock-threads` — no secret named `PROMBOT_LOCKTHREADS_TOKEN` _(needs a repository secret)_ |
| prometheus/prometheus | `prombench.yml` | pass | — |
| prometheus/prometheus | `repo_sync.yml` | expected failure | `actions/checkout` — step exited with status 1 · `Error response from daemon: container e9aa82bc55dd5943986a38d169e09ef81a04dc94a2b19562126e259768362c04 is not running` _(needs a git credential (the sweep is token-less))_ |
| prometheus/prometheus | `scorecards.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| prometheus/prometheus | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| python/cpython | `add-issue-header.yml` | **fail** | `actions/github-script` — step exited with status 1 · `}` |
| python/cpython | `lint.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| python/cpython | `mypy.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| python/cpython | `new-bugs-announce-notifier.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `[command]/usr/bin/tar xz --strip 1 --warning=no-unknown-keyword --overwrite -C /workspace/.ci/temp/4664248e-4a50-465c-8417-11490037f8ac -f /workspace/.ci/temp/e9f94f24-3cba-4def-aee0-d6c90c446c9c` |
| python/cpython | `require-pr-label.yml` | **fail** | `mheap/github-action-required-labels` — step exited with status 1 · `Error: Not Found` |
| python/cpython | `reusable-check-c-api-docs.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| python/cpython | `reusable-check-html-ids.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| python/cpython | `reusable-cifuzz.yml` | **fail** | `Dockerfile` — unsafe action path `../../../build_fuzzers.Dockerfile`: every path component must be a normal name |
| python/cpython | `reusable-context.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Download from "https://github.com/actions/python-versions/releases/download/3.14.7-31064857500/python-3.14.7-linux-24.04-x64.tar.gz"` |
| python/cpython | `reusable-docs.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-emscripten.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-install.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-san.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-wasi.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| python/cpython | `verify-ensurepip-wheels.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| python/cpython | `verify-expat.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| rails/rails | `check-markdown-api.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| rails/rails | `devcontainer-shellcheck.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| rails/rails | `devcontainer-smoke-test.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| rails/rails | `labeler.yml` | pass | — |
| rails/rails | `more-info-needed.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| rails/rails | `rail_inspector.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| rails/rails | `rails-new-docker.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| rails/rails | `rails_releaser_tests.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| rails/rails | `release.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| rails/rails | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| rust-lang/cargo | `audit.yml` | not lowered | `continue_on_error.expression` |
| rust-lang/cargo | `contrib.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| rust-lang/cargo | `release.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| sharkdp/bat | `require-changelog-for-PRs.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| tokio-rs/tokio | `audit.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| tokio-rs/tokio | `labeler.yml` | pass | — |
| tokio-rs/tokio | `loom.yml` | expected failure | `actions/checkout` — step exited with status 1 · `Removing HTTP extra header` _(needs a git credential (the sweep is token-less))_ |
| tokio-rs/tokio | `pr-audit.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| tokio-rs/tokio | `stress-test.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'core\.sshCommand' && git config --local --unset-all 'core.sshCommand' \|\| :"` _(needs a git credential (the sweep is token-less))_ |
| tokio-rs/tokio | `uring-kernel-version-test.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| vercel/next.js | `automated_code_review.yml` | not lowered | — |
| vercel/next.js | `code_freeze.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Acquiring 20.20.2 - x64 from https://nodejs.org/dist/v20.20.2/node-v20.20.2-linux-x64.tar.gz` |
| vercel/next.js | `create_release_branch.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Acquiring 20.20.2 - x64 from https://nodejs.org/dist/v20.20.2/node-v20.20.2-linux-x64.tar.gz` |
| vercel/next.js | `issue_lock.yml` | **fail** | `dessant/lock-threads` — step exited with status 1 |
| vercel/next.js | `issue_reopen.yml` | pass | — |
| vercel/next.js | `issue_stale.yml` | expected failure | `actions/stale` — no secret named `STALE_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `issue_wrong_template.yml` | pass | — |
| vercel/next.js | `popular.yml` | expected failure | `actions/checkout` — no secret named `SLACK_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `pr_ci_comment.yml` | pass | — |
| vercel/next.js | `pr_stack_optimizer.yml` | expected failure | `withgraphite/graphite-ci-action` — no secret named `GRAPHITE_CI_OPTIMIZER_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `pull_request_auto_label.yml` | pass | — |
| vercel/next.js | `release-next-rspack.yml` | not lowered | `action.nested_local`, `runs_on.expression` |
| vercel/next.js | `retry_deploy_test.yml` | pass | — |
| vercel/next.js | `retry_test.yml` | pass | — |
| vercel/next.js | `rspack-update-tests-manifest.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `sync_backport_canary_release.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git config --local --name-only --get-regexp ^includeIf\.gitdir:` _(needs a git credential (the sweep is token-less))_ |
| vercel/next.js | `test_e2e_project_reset_cron.yml` | expected failure | `actions/checkout` — no secret named `VERCEL_ADAPTER_TEST_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `test_examples.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| vercel/next.js | `triage.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| vercel/next.js | `trigger_release.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `[command]/usr/bin/tar xz --strip 1 --warning=no-unknown-keyword --overwrite -C /workspace/.ci/temp/4f6f1f64-1147-4415-bd52-6b2bd896e8fa -f /workspace/.ci/temp/a0ca5893-dcfb-454d-9962-078381583c52` |
| vercel/next.js | `turbopack-update-tests-manifest.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_fonts_data.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_react.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_react_poller.yml` | pass | — |
| vercel/next.js | `upload-tests-manifest.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| vercel/next.js | `upload_preview_tarballs.yml` | pass | — |
