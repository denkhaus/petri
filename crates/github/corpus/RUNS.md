# Corpus run sweep

The run-time counterpart of REPORT.md: every in-scope workflow that lowers, run end to end with `run:` scripts stubbed to `true` and `uses:` steps real. The outcome measures the runtime tier — checkout, action staging and execution, artifact and cache backends, cross-job flow — not the corpus projects' own builds.

Sweep configuration: host scopes rewritten to the pinned runner images `ghcr.io/lithoscomputer/ubuntu-24.04:slim-66c538cd3ef8` (22.04/26.04 variants by label), matrices capped to their first leg, each workflow capped at 300s wall clock, parallelism 6. Identity: `github.sha` is the repo's pinned corpus commit (`corpus-pins.txt`) — the sweep's analog of `default_params` reading HEAD — so `checkout` fetches real state; a dummy `GITHUB_TOKEN` keeps the sweep token-less by policy.

249 workflows in scope; 240 lower and were run.

| Result (of the 240 run) | Count | Share |
|---|---|---|
| passed | 74 | 31% |
| **failed on a runtime-tier gap** | 114 | 48% |
| expected failure (server-coupled) | 52 | 22% |
| **timed out** | 0 | 0% |

## First failures — gaps, ranked

| First failing step · class | Workflows |
|---|---|
| `actions/setup-node · exit_status:1` | 26 |
| `actions/setup-python · exit_status:1` | 13 |
| `astral-sh/setup-uv · exit_status:1` | 12 |
| `actions/setup-go · exit_status:1` | 8 |
| `actions/stale · exit_status:1` | 7 |
| `noop · env_acquire` | 5 |
| `actions/github-script · exit_status:1` | 4 |
| `noop` | 4 |
| `ghcr.io/ossf/scorecard-action:v2.4.4 · exit_status:1` | 3 |
| `github/codeql-action/init · bad_config` | 3 |
| `actions/upload-artifact · exit_status:1` | 2 |
| `bufbuild/buf-setup-action · bad_config` | 2 |
| `dessant/lock-threads · exit_status:1` | 2 |
| `docker/login-action · exit_status:1` | 2 |
| `github/gh-aw-actions/setup · exit_status:1` | 2 |
| `release-drafter/release-drafter · exit_status:1` | 2 |
| `rust-lang/crates-io-auth-action · exit_status:1` | 2 |
| `Dockerfile · action_image` | 1 |
| `Dockerfile · exit_status:126` | 1 |
| `JamesIves/github-pages-deploy-action · exit_status:1` | 1 |
| `actions/cache/restore · bad_config` | 1 |
| `actions/publish-immutable-action · exit_status:1` | 1 |
| `actions/upload-artifact` | 1 |
| `dorny/paths-filter · exit_status:1` | 1 |
| `github/checkout · checkout` | 1 |
| `github/checkout · exit_status:1` | 1 |
| `github/issue-labeler · exit_status:1` | 1 |
| `gr2m/create-or-update-pull-request-action · exit_status:1` | 1 |
| `j178/prek-action · exit_status:1` | 1 |
| `mheap/github-action-required-labels · exit_status:1` | 1 |
| `mymindstorm/setup-emsdk · exit_status:1` | 1 |
| `run: · env_acquire` | 1 |

## Expected failures — server-coupled, ranked

First failures no token-less local run can fix: OIDC, GitHub App and repository secrets, git credentials for the real checkout action, third-party SaaS backends, cross-run artifact reads. Kept out of the gap ranking so it never drowns in server-bound noise.

| First failing step · why | Workflows |
|---|---|
| `actions/checkout · needs a git credential (the sweep is token-less)` | 21 |
| `actions/create-github-app-token · needs a repository secret` | 6 |
| `actions/setup-node · needs a repository secret` | 5 |
| `actions/checkout · needs a repository secret` | 3 |
| `open-security-tools/ost-simple-sts · needs a repository secret` | 3 |
| `actions/download-artifact · cross-run artifact download (REST API)` | 2 |
| `actions/github-script · needs a repository secret` | 2 |
| `github/codeql-action/upload-sarif · uploads to GitHub code scanning` | 2 |
| `actions/stale · needs a repository secret` | 1 |
| `dessant/lock-threads · needs a repository secret` | 1 |
| `docker.io/chko/docker-pushrm:1 · needs a repository secret` | 1 |
| `google-github-actions/auth · needs a repository secret` | 1 |
| `nodejs/node-pr-labeler · needs a repository secret` | 1 |
| `peter-evans/create-pull-request · needs a repository secret` | 1 |
| `tsickert/discord-webhook · needs a repository secret` | 1 |
| `withgraphite/graphite-ci-action · needs a repository secret` | 1 |

## Every workflow

| Repository | Workflow | Result | First failure |
|---|---|---|---|
| actions/checkout | `check-dist.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `[command]/usr/bin/tar xz --strip 1 --warning=no-unknown-keyword --overwrite -C /workspace/.ci/temp/a69962ef-ffb7-4f25-b2e1-a087cd3999d3 -f /workspace/.ci/temp/a092a88c-34fe-4ada-94c3-b570186cc9b1` |
| actions/checkout | `codeql-analysis.yml` | **fail** | `github/codeql-action/init` — step config is invalid: unsafe action path `../lib/init-entry.js`: every path component must be a normal name |
| actions/checkout | `licensed.yml` | pass | — |
| actions/checkout | `publish-immutable-actions.yml` | **fail** | `actions/publish-immutable-action` — step exited with status 1 · `Error: Could not find Repository.` |
| actions/checkout | `update-main-version.yml` | pass | — |
| actions/checkout | `update-test-ubuntu-git.yml` | **fail** | `docker/login-action` — step exited with status 1 · `Error: Unable to locate executable file: docker. Please verify either the file path exists or the file can be found within a directory specified by the PATH environment variable. Also check the file m…` |
| astral-sh/ruff | `build-docker.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `build-wasm.yml` | pass | — |
| astral-sh/ruff | `daily_fuzz.yaml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/ruff | `memory_report.yaml` | **fail** | `actions/setup-python` — step exited with status 1 · `Version 3.14 was not found in the local cache` |
| astral-sh/ruff | `notify-dependents.yml` | expected failure | `actions/github-script` — no secret named `RUFF_PRE_COMMIT_PAT` _(needs a repository secret)_ |
| astral-sh/ruff | `publish-crates.yml` | **fail** | `rust-lang/crates-io-auth-action` — step exited with status 1 · `Error: Please ensure the 'id-token' permission is set to 'write' in your workflow. For more information, see: https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-dep…` |
| astral-sh/ruff | `publish-docs.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `publish-mirror.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/ruff | `publish-playground.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `[command]/usr/bin/tar xz --strip 1 --warning=no-unknown-keyword --overwrite -C /workspace/.ci/temp/0e1b0d69-9868-4842-af0b-0c49d6a3cb45 -f /workspace/.ci/temp/22c0ea10-ba75-48c6-aa6e-854691612a07` |
| astral-sh/ruff | `publish-pypi.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/ruff | `publish-ty-playground.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Extracting ...` |
| astral-sh/ruff | `publish-versions.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/ruff | `publish-wasm.yml` | expected failure | `actions/download-artifact` — step exited with status 1 · `Error: Unable to download artifact(s): Unable to get the ACTIONS_RUNTIME_TOKEN env variable` _(cross-run artifact download (REST API))_ |
| astral-sh/ruff | `ty-ecosystem-analyzer.yaml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/ruff | `ty-ecosystem-report.yaml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/ruff | `typing_conformance.yaml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `bench.yml` | not lowered | `runs_on.unknown` |
| astral-sh/uv | `build-docker.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `check-docs.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `check-fmt.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `check-generated-files.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `check-lock.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `check-publish.yml` | pass | — |
| astral-sh/uv | `check-release.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `check-zizmor.yml` | expected failure | `github/codeql-action/upload-sarif` — step config is invalid: unsafe action path `../lib/upload-sarif-entry.js`: every path component must be a normal name _(uploads to GitHub code scanning)_ |
| astral-sh/uv | `diagnose-workflow-failure.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `[command]/usr/bin/tar xz --strip 1 --warning=no-unknown-keyword --overwrite -C /workspace/.ci/temp/b54af41a-3fed-4aa6-85de-dc82dcd8d011 -f /workspace/.ci/temp/95c501a6-0ffb-4494-b4ea-15fe67e2e165` |
| astral-sh/uv | `fix-bug.yml` | expected failure | `actions/download-artifact` — step exited with status 1 · `Error: Unable to download artifact(s): Bad credentials - https://docs.github.com/rest` _(cross-run artifact download (REST API))_ |
| astral-sh/uv | `issue-triage.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `plan.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `promote-pull-request.yml` | pass | — |
| astral-sh/uv | `publish-crates.yml` | **fail** | `rust-lang/crates-io-auth-action` — step exited with status 1 · `Error: Please ensure the 'id-token' permission is set to 'write' in your workflow. For more information, see: https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-dep…` |
| astral-sh/uv | `publish-docs.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `publish-mirror.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/uv | `publish-pypi.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `publish-versions.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/uv | `pull-request-conflicts.yml` | **fail** | `actions/upload-artifact` — step config failed to resolve |
| astral-sh/uv | `pull-request-labels.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `pull-request-security-review.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `rebase-conflicted-pull-request.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Acquiring 24.20.0 - x64 from https://nodejs.org/dist/v24.20.0/node-v24.20.0-linux-x64.tar.gz` |
| astral-sh/uv | `release-prepare.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `reproduce-bug.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `sync-python-releases.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `sync-uv-dev.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `sync-uv-security.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `test-ecosystem.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive git config --local --show-origin --name-only --get-regexp remote.origin.url` _(needs a git credential (the sweep is token-less))_ |
| astral-sh/uv | `update-issue-context.yml` | pass | — |
| astral-sh/uv | `update-pull-request-parent.yml` | pass | — |
| cli/cli | `agentics-maintenance.yml` | **fail** | `actions/cache/restore` — step config is invalid: unsafe action path `../dist/restore-only/index.js`: every path component must be a normal name |
| cli/cli | `bump-go.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Error: The specified go version file at: go.mod does not exist` |
| cli/cli | `codeql.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Error: The specified go version file at: go.mod does not exist` |
| cli/cli | `copilot-setup-steps.yml` | pass | — |
| cli/cli | `dependabot-triage.lock.yml` | **fail** | `github/gh-aw-actions/setup` — step exited with status 1 · `Failed to run setup.sh: spawnSync /workspace/.ci/github/actions/github/gh-aw-actions/423b3dc04bbf1b1797194a4a75aa5cf5d0d4f5b3/setup/setup.sh EACCES` |
| cli/cli | `govulncheck.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Error: The specified go version file at: go.mod does not exist` |
| cli/cli | `issue-triage.lock.yml` | **fail** | `github/gh-aw-actions/setup` — step exited with status 1 · `Failed to run setup.sh: spawnSync /workspace/.ci/github/actions/github/gh-aw-actions/423b3dc04bbf1b1797194a4a75aa5cf5d0d4f5b3/setup/setup.sh EACCES` |
| cli/cli | `lint.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Error: The specified go version file at: go.mod does not exist` |
| cli/cli | `triage-issues.yml` | pass | — |
| cli/cli | `triage-pull-requests.yml` | pass | — |
| cli/cli | `triage-scheduled-tasks.yml` | pass | — |
| denoland/deno | `cargo_publish.generated.yml` | expected failure | `actions/checkout` — no secret named `DENOBOT_PAT` _(needs a repository secret)_ |
| denoland/deno | `create_prerelease_tag.generated.yml` | expected failure | `actions/checkout` — no secret named `DENOBOT_PAT` _(needs a repository secret)_ |
| denoland/deno | `post_publish.generated.yml` | expected failure | `google-github-actions/auth` — no secret named `GCP_SA_KEY` _(needs a repository secret)_ |
| denoland/deno | `pr.generated.yml` | pass | — |
| denoland/deno | `start_release.generated.yml` | pass | — |
| denoland/deno | `version_bump.generated.yml` | expected failure | `actions/checkout` — no secret named `DENOBOT_PAT` _(needs a repository secret)_ |
| django/django | `benchmark.yml` | pass | — |
| django/django | `check-migrations.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Download from "https://github.com/actions/python-versions/releases/download/3.14.7-31064857500/python-3.14.7-linux-24.04-x64.tar.gz"` |
| django/django | `check_commit_messages.yml` | pass | — |
| django/django | `check_pr_quality.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| django/django | `coverage_comment.yml` | pass | — |
| django/django | `coverage_tests.yml` | pass | — |
| django/django | `docs.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Download from "https://github.com/actions/python-versions/releases/download/3.14.7-31064857500/python-3.14.7-linux-24.04-x64.tar.gz"` |
| django/django | `labels.yml` | **fail** | `actions/github-script` — step exited with status 1 · `    at Module._load (node:internal/modules/cjs/loader:1396:12)` |
| django/django | `linters.yml` | expected failure | `github/codeql-action/upload-sarif` — step config is invalid: unsafe action path `../lib/upload-sarif-action.js`: every path component must be a normal name _(uploads to GitHub code scanning)_ |
| django/django | `new_contributor_pr.yml` | pass | — |
| django/django | `playwright.yml` | pass | — |
| django/django | `postgis.yml` | pass | — |
| django/django | `python_matrix.yml` | pass | — |
| django/django | `schedules.yml` | expected failure | `actions/github-script` — no secret named `SCHEDULE_WORKFLOW_TOKEN` _(needs a repository secret)_ |
| django/django | `screenshots.yml` | pass | — |
| facebook/react | `compiler_discord_notify.yml` | pass | — |
| facebook/react | `compiler_playground.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `(Use `node --trace-deprecation ...` to show where the warning was created)` |
| facebook/react | `compiler_prereleases.yml` | expected failure | `actions/setup-node` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_prereleases_manual.yml` | expected failure | `actions/setup-node` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_prereleases_nightly.yml` | expected failure | `actions/setup-node` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_rust.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `(Use `node --trace-deprecation ...` to show where the warning was created)` |
| facebook/react | `compiler_typescript.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `(Use `node --trace-deprecation ...` to show where the warning was created)` |
| facebook/react | `devtools_discord_notify.yml` | pass | — |
| facebook/react | `devtools_regression_tests.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `(Use `node --trace-deprecation ...` to show where the warning was created)` |
| facebook/react | `runtime_build_and_test.yml` | not lowered | `step.background` |
| facebook/react | `runtime_commit_artifacts.yml` | **fail** | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: build/. No artifacts will be uploaded.` |
| facebook/react | `runtime_discord_notify.yml` | pass | — |
| facebook/react | `runtime_eslint_plugin_e2e.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| facebook/react | `runtime_fuzz_tests.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `(Use `node --trace-deprecation ...` to show where the warning was created)` |
| facebook/react | `runtime_release_from_ci.yml` | expected failure | `tsickert/discord-webhook` — no secret named `DISCORD_WEBHOOK_URL` _(needs a repository secret)_ |
| facebook/react | `runtime_sizebot_comment.yml` | pass | — |
| facebook/react | `shared_check_maintainer.yml` | **fail** | `actions/github-script` — step exited with status 1 · `Error: Unhandled error: HttpError: Bad credentials` |
| facebook/react | `shared_cleanup_merged_branch_caches.yml` | pass | — |
| facebook/react | `shared_cleanup_stale_branch_caches.yml` | pass | — |
| facebook/react | `shared_close_direct_sync_branch_prs.yml` | **fail** | `actions/github-script` — step exited with status 1 · `Error: Unhandled error: SyntaxError: Unexpected token ';'` |
| facebook/react | `shared_label_core_team_prs.yml` | pass | — |
| facebook/react | `shared_lint.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `(Use `node --trace-deprecation ...` to show where the warning was created)` |
| facebook/react | `shared_stale.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials` |
| hashicorp/terraform | `backport.yml` | **fail** | `run:` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| hashicorp/terraform | `changelog-validation.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| hashicorp/terraform | `checks.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Error: Unable to find Go version 'null' for platform linux and architecture x64.` |
| hashicorp/terraform | `enforce-changelog.yml` | **fail** | `dorny/paths-filter` — step exited with status 1 · `Error: The process 'git rev-parse --abbrev-ref HEAD' failed with exit code 128` |
| hashicorp/terraform | `equivalence-test-diff.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Error: Unable to find Go version 'null' for platform linux and architecture x64.` |
| hashicorp/terraform | `equivalence-test-manual-update.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| hashicorp/terraform | `equivalence-test-update.yml` | pass | — |
| hashicorp/terraform | `issue-comment-created.yml` | pass | — |
| hashicorp/terraform | `lock.yml` | **fail** | `dessant/lock-threads` — step exited with status 1 · `Error: Bad credentials - https://docs.github.com/rest` |
| nodejs/node | `auto-start-ci.yml` | pass | — |
| nodejs/node | `build-tarball.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Version ~3.14.0-0 was not found in the local cache` |
| nodejs/node | `close-stalled.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| nodejs/node | `codeql.yml` | **fail** | `github/codeql-action/init` — step config is invalid: unsafe action path `../lib/init-entry.js`: every path component must be a normal name |
| nodejs/node | `comment-labeled.yml` | pass | — |
| nodejs/node | `commit-lint.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Attempt to resolve LTS alias from manifest...` |
| nodejs/node | `commit-queue.yml` | pass | — |
| nodejs/node | `coverage-linux-without-intl.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Version ~3.14.0-0 was not found in the local cache` |
| nodejs/node | `coverage-linux.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Version ~3.14.0-0 was not found in the local cache` |
| nodejs/node | `create-release-proposal.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `daily-wpt-fyi.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Version ~3.14.0-0 was not found in the local cache` |
| nodejs/node | `daily.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: Bad credentials` |
| nodejs/node | `doc.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: Bad credentials` |
| nodejs/node | `find-inactive-collaborators.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: Bad credentials` |
| nodejs/node | `find-inactive-tsc.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'core\.sshCommand' && git config --local --unset-all 'core.sshCommand' \|\| :"` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `label-flaky-test-issue.yml` | pass | — |
| nodejs/node | `label-pr.yml` | expected failure | `nodejs/node-pr-labeler` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `license-builder.yml` | **fail** | `gr2m/create-or-update-pull-request-action` — step exited with status 1 · `Error: Command failed with exit code 128 (Unknown system error -128): git status` |
| nodejs/node | `lint-release-proposal.yml` | pass | — |
| nodejs/node | `linters.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: Bad credentials` |
| nodejs/node | `major-release.yml` | pass | — |
| nodejs/node | `nix-changes-comment.yml` | pass | — |
| nodejs/node | `notify-on-push.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: Bad credentials` |
| nodejs/node | `notify-on-review-wanted.yml` | pass | — |
| nodejs/node | `post-release.yml` | pass | — |
| nodejs/node | `scorecard.yml` | **fail** | `ghcr.io/ossf/scorecard-action:v2.4.4` — step exited with status 1 · `2026/08/28 17:32:22 scorecard had an error: repo unreachable: GET https://api.github.com/repos/nodejs/node: 401 Bad credentials []` |
| nodejs/node | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| nodejs/node | `test-internet.yml` | pass | — |
| nodejs/node | `test-linux-quic.yml` | not lowered | `action.local_missing` |
| nodejs/node | `test-linux.yml` | not lowered | `action.local_missing` |
| nodejs/node | `test-shared.yml` | not lowered | `runs_on.expression` |
| nodejs/node | `timezone-update.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'core\.sshCommand' && git config --local --unset-all 'core.sshCommand' \|\| :"` _(needs a git credential (the sweep is token-less))_ |
| nodejs/node | `tools.yml` | expected failure | `peter-evans/create-pull-request` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `update-openssl.yml` | pass | — |
| nodejs/node | `update-v8.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: Bad credentials` |
| nodejs/node | `update-wpt.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: Bad credentials` |
| ohmyzsh/ohmyzsh | `dependencies.yml` | expected failure | `actions/create-github-app-token` — no secret named `OHMYZSH_CLIENT_ID` _(needs a repository secret)_ |
| ohmyzsh/ohmyzsh | `main.yml` | pass | — |
| ohmyzsh/ohmyzsh | `project.yml` | expected failure | `actions/create-github-app-token` — no secret named `OHMYZSH_CLIENT_ID` _(needs a repository secret)_ |
| ohmyzsh/ohmyzsh | `scorecard.yml` | **fail** | `ghcr.io/ossf/scorecard-action:v2.4.4` — step exited with status 1 · `2026/08/28 17:32:29 scorecard had an error: repo unreachable: GET https://api.github.com/repos/ohmyzsh/ohmyzsh: 401 Bad credentials []` |
| pola-rs/polars | `benchmark-remote.yml` | not lowered | `runs_on.unknown` |
| pola-rs/polars | `benchmark.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Download from "https://github.com/actions/python-versions/releases/download/3.14.7-31064857500/python-3.14.7-linux-24.04-x64.tar.gz"` |
| pola-rs/polars | `changes-dsl-labeler.yml` | pass | — |
| pola-rs/polars | `clear-caches.yml` | pass | — |
| pola-rs/polars | `docs-python.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'core\.sshCommand' && git config --local --unset-all 'core.sshCommand' \|\| :"` _(needs a git credential (the sweep is token-less))_ |
| pola-rs/polars | `docs-rust.yml` | **fail** | `JamesIves/github-pages-deploy-action` — step exited with status 1 · `Notice: Deployment failed! ❌` |
| pola-rs/polars | `issue-labeler.yml` | **fail** | `github/issue-labeler` — step exited with status 1 · `Error: HttpError: Bad credentials` |
| pola-rs/polars | `lint-global.yml` | pass | — |
| pola-rs/polars | `lint-python.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Download from "https://github.com/actions/python-versions/releases/download/3.10.21-31661269155/python-3.10.21-linux-24.04-x64.tar.gz"` |
| pola-rs/polars | `lint-rust.yml` | pass | — |
| pola-rs/polars | `pr-labeler.yml` | **fail** | `release-drafter/release-drafter` — step exited with status 1 · `Error: Invalid config file` |
| pola-rs/polars | `release-drafter.yml` | **fail** | `release-drafter/release-drafter` — step exited with status 1 · `Error: Invalid config file` |
| pola-rs/polars | `release-rust.yml` | pass | — |
| pola-rs/polars | `test-bytecode-parser.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Download from "https://github.com/actions/python-versions/releases/download/3.10.21-31661269155/python-3.10.21-linux-24.04-x64.tar.gz"` |
| pola-rs/polars | `test-pyodide.yml` | **fail** | `mymindstorm/setup-emsdk` — step exited with status 1 · `[command]/workspace/.ci/temp/d0d04bc1-058e-42ea-8aae-e11b82575f88/emsdk-main/emsdk install 3.1.58` |
| prometheus/prometheus | `approve-workflows.yml` | pass | — |
| prometheus/prometheus | `automerge-dependabot.yml` | pass | — |
| prometheus/prometheus | `buf-lint.yml` | **fail** | `bufbuild/buf-setup-action` — step config is invalid: unsafe action path `./dist/main.js`: every path component must be a normal name |
| prometheus/prometheus | `buf.yml` | **fail** | `bufbuild/buf-setup-action` — step config is invalid: unsafe action path `./dist/main.js`: every path component must be a normal name |
| prometheus/prometheus | `check_release_notes.yml` | pass | — |
| prometheus/prometheus | `codeql-analysis.yml` | **fail** | `github/codeql-action/init` — step config is invalid: unsafe action path `../lib/init-entry.js`: every path component must be a normal name |
| prometheus/prometheus | `container_description.yml` | expected failure | `docker.io/chko/docker-pushrm:1` — no secret named `DOCKER_HUB_PASSWORD` _(needs a repository secret)_ |
| prometheus/prometheus | `fuzzing.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Acquiring 1.27.0 from https://github.com/actions/go-versions/releases/download/1.27.0-32325163857/go-1.27.0-linux-x64.tar.gz` |
| prometheus/prometheus | `govulncheck.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Acquiring 1.27.0 from https://github.com/actions/go-versions/releases/download/1.27.0-32325163857/go-1.27.0-linux-x64.tar.gz` |
| prometheus/prometheus | `lock.yml` | expected failure | `dessant/lock-threads` — no secret named `PROMBOT_LOCKTHREADS_TOKEN` _(needs a repository secret)_ |
| prometheus/prometheus | `prombench.yml` | pass | — |
| prometheus/prometheus | `repo_sync.yml` | **fail** | `github/checkout` — step exited with status 1 · `Error response from daemon: container 85c98378413f566fd0dd876b035ec291b59ba6195b42d96b902d06f29674a9d2 is not running` |
| prometheus/prometheus | `scorecards.yml` | **fail** | `ghcr.io/ossf/scorecard-action:v2.4.4` — step exited with status 1 · `2026/08/28 17:33:03 scorecard had an error: repo unreachable: GET https://api.github.com/repos/prometheus/prometheus: 401 Bad credentials []` |
| prometheus/prometheus | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| python/cpython | `add-issue-header.yml` | **fail** | `actions/github-script` — step exited with status 1 · `}` |
| python/cpython | `lint.yml` | **fail** | `j178/prek-action` — step exited with status 1 · `Downloaded archive to /workspace/.ci/temp/eeeece92-4ff2-4b2b-b58d-a1fe28e397c8` |
| python/cpython | `mypy.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| python/cpython | `new-bugs-announce-notifier.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Acquiring 20.20.2 - x64 from https://nodejs.org/dist/v20.20.2/node-v20.20.2-linux-x64.tar.gz` |
| python/cpython | `require-pr-label.yml` | **fail** | `mheap/github-action-required-labels` — step exited with status 1 · `Error: Not Found` |
| python/cpython | `reusable-check-c-api-docs.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Download from "https://github.com/actions/python-versions/releases/download/3.14.7-31064857500/python-3.14.7-linux-24.04-x64.tar.gz"` |
| python/cpython | `reusable-check-html-ids.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| python/cpython | `reusable-cifuzz.yml` | **fail** | `Dockerfile` — unsafe action path `../../../build_fuzzers.Dockerfile`: every path component must be a normal name |
| python/cpython | `reusable-context.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Download from "https://github.com/actions/python-versions/releases/download/3.14.7-31064857500/python-3.14.7-linux-24.04-x64.tar.gz"` |
| python/cpython | `reusable-docs.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-emscripten.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-install.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-san.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-wasi.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| python/cpython | `verify-ensurepip-wheels.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Download from "https://github.com/actions/python-versions/releases/download/3.14.7-31064857500/python-3.14.7-linux-24.04-x64.tar.gz"` |
| python/cpython | `verify-expat.yml` | pass | — |
| rails/rails | `check-markdown-api.yml` | pass | — |
| rails/rails | `devcontainer-shellcheck.yml` | pass | — |
| rails/rails | `devcontainer-smoke-test.yml` | **fail** | `docker/login-action` — step exited with status 1 · `Error: Unable to locate executable file: docker. Please verify either the file path exists or the file can be found within a directory specified by the PATH environment variable. Also check the file m…` |
| rails/rails | `labeler.yml` | pass | — |
| rails/rails | `more-info-needed.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| rails/rails | `rail_inspector.yml` | pass | — |
| rails/rails | `rails-new-docker.yml` | pass | — |
| rails/rails | `rails_releaser_tests.yml` | pass | — |
| rails/rails | `release.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: Bad credentials` |
| rails/rails | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 · `Error: Getting issues was blocked by the error: Bad credentials - https://docs.github.com/rest` |
| rust-lang/cargo | `audit.yml` | not lowered | `continue_on_error.expression` |
| rust-lang/cargo | `contrib.yml` | **fail** | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: /artifact.tar. No artifacts will be uploaded.` |
| rust-lang/cargo | `release.yml` | pass | — |
| sharkdp/bat | `require-changelog-for-PRs.yml` | pass | — |
| tokio-rs/tokio | `audit.yml` | **fail** | `Dockerfile` — step exited with status 126 · `[FATAL tini (7)] exec /entrypoint.sh failed: Permission denied` |
| tokio-rs/tokio | `labeler.yml` | pass | — |
| tokio-rs/tokio | `loom.yml` | pass | — |
| tokio-rs/tokio | `pr-audit.yml` | **fail** | `github/checkout` — could not copy `/Users/bhelmkamp/p/lithoscomputer/petri/crates/github/acceptance/../corpus/tokio-rs__tokio`: No such file or directory (os error 2) |
| tokio-rs/tokio | `stress-test.yml` | pass | — |
| tokio-rs/tokio | `uring-kernel-version-test.yml` | pass | — |
| vercel/next.js | `automated_code_review.yml` | not lowered | — |
| vercel/next.js | `code_freeze.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Falling back to download directly from Node` |
| vercel/next.js | `create_release_branch.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Acquiring 20.20.2 - x64 from https://nodejs.org/dist/v20.20.2/node-v20.20.2-linux-x64.tar.gz` |
| vercel/next.js | `issue_lock.yml` | **fail** | `dessant/lock-threads` — step exited with status 1 · `Error: Bad credentials` |
| vercel/next.js | `issue_reopen.yml` | pass | — |
| vercel/next.js | `issue_stale.yml` | expected failure | `actions/stale` — no secret named `STALE_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `issue_wrong_template.yml` | pass | — |
| vercel/next.js | `popular.yml` | expected failure | `actions/setup-node` — no secret named `SLACK_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `pr_ci_comment.yml` | pass | — |
| vercel/next.js | `pr_stack_optimizer.yml` | expected failure | `withgraphite/graphite-ci-action` — no secret named `GRAPHITE_CI_OPTIMIZER_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `pull_request_auto_label.yml` | pass | — |
| vercel/next.js | `release-next-rspack.yml` | not lowered | `action.nested_local`, `runs_on.expression` |
| vercel/next.js | `retry_deploy_test.yml` | pass | — |
| vercel/next.js | `retry_test.yml` | pass | — |
| vercel/next.js | `rspack-update-tests-manifest.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `sync_backport_canary_release.yml` | expected failure | `actions/checkout` — step exited with status 1 · `[command]/usr/bin/git submodule foreach --recursive sh -c "git config --local --name-only --get-regexp 'http\.https\:\/\/github\.com\/\.extraheader' && git config --local --unset-all 'http.https://git…` _(needs a git credential (the sweep is token-less))_ |
| vercel/next.js | `test_e2e_project_reset_cron.yml` | expected failure | `actions/setup-node` — no secret named `VERCEL_ADAPTER_TEST_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `test_examples.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: The specified node version file at: /workspace/repo/.node-version does not exist` |
| vercel/next.js | `triage.yml` | pass | — |
| vercel/next.js | `trigger_release.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Acquiring 20.20.2 - x64 from https://nodejs.org/dist/v20.20.2/node-v20.20.2-linux-x64.tar.gz` |
| vercel/next.js | `turbopack-update-tests-manifest.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_fonts_data.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_react.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_react_poller.yml` | pass | — |
| vercel/next.js | `upload-tests-manifest.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: The specified node version file at: /workspace/repo/.node-version does not exist` |
| vercel/next.js | `upload_preview_tarballs.yml` | pass | — |
