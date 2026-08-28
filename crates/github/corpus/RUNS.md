# Corpus run sweep

The run-time counterpart of REPORT.md: every in-scope workflow that lowers, run end to end with `run:` scripts stubbed to `true` and `uses:` steps real. The outcome measures the runtime tier — checkout, action staging and execution, artifact and cache backends, cross-job flow — not the corpus projects' own builds.

Sweep configuration: host scopes rewritten to the pinned runner images `ghcr.io/lithoscomputer/ubuntu-24.04:slim-66c538cd3ef8` (22.04/26.04 variants by label), matrices capped to their first leg, each workflow capped at 300s wall clock, parallelism 6. Identity: `github.sha` is the repo's pinned corpus commit (`corpus-pins.txt`) — the sweep's analog of `default_params` reading HEAD — so `checkout` fetches real state; a real `GITHUB_TOKEN` (`PETRI_SWEEP_TOKEN`) authenticated actions' API calls, so no anonymous rate limit applied.

249 workflows in scope; 240 lower and were run.

| Result (of the 240 run) | Count | Share |
|---|---|---|
| passed | 102 | 42% |
| **failed on a runtime-tier gap** | 70 | 29% |
| expected failure (server-coupled) | 50 | 21% |
| **timed out** | 18 | 8% |

## First failures — gaps, ranked

| First failing step · class | Workflows |
|---|---|
| `astral-sh/setup-uv · exit_status:1` | 16 |
| `actions/setup-node · exit_status:1` | 8 |
| `actions/setup-go · exit_status:1` | 7 |
| `actions/checkout · exit_status:1` | 5 |
| `actions/upload-artifact · exit_status:1` | 5 |
| `noop` | 4 |
| `noop · env_acquire` | 4 |
| `actions/github-script · exit_status:1` | 3 |
| `github/gh-aw-actions/setup · exit_status:1` | 3 |
| `Dockerfile · exit_status:126` | 2 |
| `bufbuild/buf-lint-action · exit_status:1` | 2 |
| `docker/login-action · exit_status:1` | 2 |
| `ghcr.io/ossf/scorecard-action:v2.4.4 · exit_status:1` | 2 |
| `Dockerfile · action_image` | 1 |
| `actions/upload-artifact` | 1 |
| `docker/setup-buildx-action · exit_status:1` | 1 |
| `dorny/paths-filter · exit_status:1` | 1 |
| `github/checkout · exit_status:1` | 1 |
| `j178/prek-action · exit_status:1` | 1 |
| `run: · env_acquire` | 1 |

## Expected failures — server-coupled, ranked

First failures no token-less local run can fix: OIDC, GitHub App and repository secrets, git credentials for the real checkout action, third-party SaaS backends, cross-run artifact reads. Kept out of the gap ranking so it never drowns in server-bound noise.

| First failing step · why | Workflows |
|---|---|
| `actions/create-github-app-token · needs a repository secret` | 9 |
| `actions/setup-node · needs a repository secret` | 5 |
| `actions/checkout · needs a repository secret` | 3 |
| `actions/download-artifact · cross-run artifact download (REST API)` | 3 |
| `actions/github-script · needs a repository secret` | 3 |
| `gr2m/create-or-update-pull-request-action · needs a repository secret` | 3 |
| `open-security-tools/ost-simple-sts · needs a repository secret` | 3 |
| `dessant/lock-threads · mutates issues over the API` | 2 |
| `peter-evans/create-pull-request · needs a repository secret` | 2 |
| `release-drafter/release-drafter · mutates releases over the API` | 2 |
| `rust-lang/crates-io-auth-action · OIDC token exchange` | 2 |
| `JamesIves/github-pages-deploy-action · pushes a deployment branch` | 1 |
| `actions/publish-immutable-action · publishes to GitHub's registry` | 1 |
| `actions/stale · needs a repository secret` | 1 |
| `dessant/lock-threads · needs a repository secret` | 1 |
| `docker.io/chko/docker-pushrm:1 · needs a repository secret` | 1 |
| `github/issue-labeler · mutates issues over the API` | 1 |
| `google-github-actions/auth · needs a repository secret` | 1 |
| `gr2m/create-or-update-pull-request-action · mutates pull requests over the API` | 1 |
| `mheap/github-action-required-labels · reads the triggering pull request (no local event)` | 1 |
| `nodejs/node-pr-labeler · needs a repository secret` | 1 |
| `rubygems/configure-rubygems-credentials · OIDC token exchange` | 1 |
| `tsickert/discord-webhook · needs a repository secret` | 1 |
| `withgraphite/graphite-ci-action · needs a repository secret` | 1 |

## Every workflow

| Repository | Workflow | Result | First failure |
|---|---|---|---|
| actions/checkout | `check-dist.yml` | pass | — |
| actions/checkout | `codeql-analysis.yml` | **timeout** | — |
| actions/checkout | `licensed.yml` | pass | — |
| actions/checkout | `publish-immutable-actions.yml` | expected failure | `actions/publish-immutable-action` — step exited with status 1 · `Error: Could not find Repository.` _(publishes to GitHub's registry)_ |
| actions/checkout | `update-main-version.yml` | pass | — |
| actions/checkout | `update-test-ubuntu-git.yml` | **fail** | `docker/login-action` — step exited with status 1 · `Error: Unable to locate executable file: docker. Please verify either the file path exists or the file can be found within a directory specified by the PATH environment variable. Also check the file m…` |
| astral-sh/ruff | `build-docker.yml` | **fail** | `docker/setup-buildx-action` — step exited with status 1 · `Error: ERROR: failed to initialize builder builder-acde5e2c-2815-4ab0-b304-7db665c526fa (builder-acde5e2c-2815-4ab0-b304-7db665c526fa0): failed to connect to the docker API at unix:///var/run/docker.s…` |
| astral-sh/ruff | `build-wasm.yml` | pass | — |
| astral-sh/ruff | `daily_fuzz.yaml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/ruff | `memory_report.yaml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/ruff | `notify-dependents.yml` | expected failure | `actions/github-script` — no secret named `RUFF_PRE_COMMIT_PAT` _(needs a repository secret)_ |
| astral-sh/ruff | `publish-crates.yml` | expected failure | `rust-lang/crates-io-auth-action` — step exited with status 1 · `Error: Please ensure the 'id-token' permission is set to 'write' in your workflow. For more information, see: https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-dep…` _(OIDC token exchange)_ |
| astral-sh/ruff | `publish-docs.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/ruff | `publish-mirror.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/ruff | `publish-playground.yml` | **fail** | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: playground/ruff/dist. No artifacts will be uploaded.` |
| astral-sh/ruff | `publish-pypi.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/ruff | `publish-ty-playground.yml` | **fail** | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: playground/ty/dist. No artifacts will be uploaded.` |
| astral-sh/ruff | `publish-versions.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/ruff | `publish-wasm.yml` | expected failure | `actions/download-artifact` — step exited with status 1 · `Error: Unable to download artifact(s): Unable to get the ACTIONS_RUNTIME_TOKEN env variable` _(cross-run artifact download (REST API))_ |
| astral-sh/ruff | `ty-ecosystem-analyzer.yaml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/ruff | `ty-ecosystem-report.yaml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/ruff | `typing_conformance.yaml` | **timeout** | — |
| astral-sh/uv | `bench.yml` | not lowered | `runs_on.unknown` |
| astral-sh/uv | `build-docker.yml` | **timeout** | — |
| astral-sh/uv | `check-docs.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `check-fmt.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `check-generated-files.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `check-lock.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `check-publish.yml` | pass | — |
| astral-sh/uv | `check-release.yml` | pass | — |
| astral-sh/uv | `check-zizmor.yml` | pass | — |
| astral-sh/uv | `diagnose-workflow-failure.yml` | pass | — |
| astral-sh/uv | `fix-bug.yml` | expected failure | `actions/download-artifact` — step exited with status 1 · `Error: Unable to download artifact(s): Not Found - https://docs.github.com/rest/actions/artifacts#list-workflow-run-artifacts` _(cross-run artifact download (REST API))_ |
| astral-sh/uv | `issue-triage.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `plan.yml` | pass | — |
| astral-sh/uv | `promote-pull-request.yml` | pass | — |
| astral-sh/uv | `publish-crates.yml` | expected failure | `rust-lang/crates-io-auth-action` — step exited with status 1 · `Error: Please ensure the 'id-token' permission is set to 'write' in your workflow. For more information, see: https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-dep…` _(OIDC token exchange)_ |
| astral-sh/uv | `publish-docs.yml` | **timeout** | — |
| astral-sh/uv | `publish-mirror.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/uv | `publish-pypi.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `publish-versions.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/uv | `pull-request-conflicts.yml` | **fail** | `actions/upload-artifact` — step config failed to resolve |
| astral-sh/uv | `pull-request-labels.yml` | **fail** | `actions/checkout` — step exited with status 1 · `Error: The process '/usr/bin/git' failed with exit code 128` |
| astral-sh/uv | `pull-request-security-review.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `rebase-conflicted-pull-request.yml` | **fail** | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: /rebased.bundle. No artifacts will be uploaded.` |
| astral-sh/uv | `release-prepare.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `reproduce-bug.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `sync-python-releases.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| astral-sh/uv | `sync-uv-dev.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `sync-uv-security.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `test-ecosystem.yml` | expected failure | `actions/download-artifact` — step exited with status 1 · `Error: Unable to download artifact(s): Unable to get the ACTIONS_RUNTIME_TOKEN env variable` _(cross-run artifact download (REST API))_ |
| astral-sh/uv | `update-issue-context.yml` | pass | — |
| astral-sh/uv | `update-pull-request-parent.yml` | pass | — |
| cli/cli | `agentics-maintenance.yml` | **fail** | `github/gh-aw-actions/setup` — step exited with status 1 · `Failed to run setup.sh: spawnSync /workspace/.ci/github/actions/github/gh-aw-actions/423b3dc04bbf1b1797194a4a75aa5cf5d0d4f5b3/setup/setup.sh EACCES` |
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
| denoland/deno | `pr.generated.yml` | **timeout** | — |
| denoland/deno | `start_release.generated.yml` | pass | — |
| denoland/deno | `version_bump.generated.yml` | expected failure | `actions/checkout` — no secret named `DENOBOT_PAT` _(needs a repository secret)_ |
| django/django | `benchmark.yml` | pass | — |
| django/django | `check-migrations.yml` | **timeout** | — |
| django/django | `check_commit_messages.yml` | pass | — |
| django/django | `check_pr_quality.yml` | **fail** | `actions/checkout` — step exited with status 1 · `Error: The process '/usr/bin/git' failed with exit code 1` |
| django/django | `coverage_comment.yml` | pass | — |
| django/django | `coverage_tests.yml` | pass | — |
| django/django | `docs.yml` | **timeout** | — |
| django/django | `labels.yml` | **fail** | `actions/github-script` — step exited with status 1 · `Error: Unhandled error: TypeError: Cannot read properties of undefined (reading 'title')` |
| django/django | `linters.yml` | **timeout** | — |
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
| facebook/react | `runtime_eslint_plugin_e2e.yml` | pass | — |
| facebook/react | `runtime_fuzz_tests.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `(Use `node --trace-deprecation ...` to show where the warning was created)` |
| facebook/react | `runtime_release_from_ci.yml` | expected failure | `tsickert/discord-webhook` — no secret named `DISCORD_WEBHOOK_URL` _(needs a repository secret)_ |
| facebook/react | `runtime_sizebot_comment.yml` | pass | — |
| facebook/react | `shared_check_maintainer.yml` | pass | — |
| facebook/react | `shared_cleanup_merged_branch_caches.yml` | pass | — |
| facebook/react | `shared_cleanup_stale_branch_caches.yml` | pass | — |
| facebook/react | `shared_close_direct_sync_branch_prs.yml` | **fail** | `actions/github-script` — step exited with status 1 · `Error: Unhandled error: SyntaxError: Unexpected token ';'` |
| facebook/react | `shared_label_core_team_prs.yml` | pass | — |
| facebook/react | `shared_lint.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `(Use `node --trace-deprecation ...` to show where the warning was created)` |
| facebook/react | `shared_stale.yml` | pass | — |
| hashicorp/terraform | `backport.yml` | **fail** | `run:` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| hashicorp/terraform | `changelog-validation.yml` | pass | — |
| hashicorp/terraform | `checks.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Error: Unable to find Go version 'null' for platform linux and architecture x64.` |
| hashicorp/terraform | `enforce-changelog.yml` | **fail** | `dorny/paths-filter` — step exited with status 1 · `Error: The process 'git rev-parse --abbrev-ref HEAD' failed with exit code 128` |
| hashicorp/terraform | `equivalence-test-diff.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Error: Unable to find Go version 'null' for platform linux and architecture x64.` |
| hashicorp/terraform | `equivalence-test-manual-update.yml` | **fail** | `actions/setup-go` — step exited with status 1 · `Error: Unable to find Go version 'null' for platform linux and architecture x64.` |
| hashicorp/terraform | `equivalence-test-update.yml` | pass | — |
| hashicorp/terraform | `issue-comment-created.yml` | pass | — |
| hashicorp/terraform | `lock.yml` | expected failure | `dessant/lock-threads` — step exited with status 1 · `Error: Must have admin rights to Repository. - https://docs.github.com/rest/issues/issues#lock-an-issue` _(mutates issues over the API)_ |
| nodejs/node | `auto-start-ci.yml` | pass | — |
| nodejs/node | `build-tarball.yml` | **timeout** | — |
| nodejs/node | `close-stalled.yml` | pass | — |
| nodejs/node | `codeql.yml` | **timeout** | — |
| nodejs/node | `comment-labeled.yml` | pass | — |
| nodejs/node | `commit-lint.yml` | pass | — |
| nodejs/node | `commit-queue.yml` | pass | — |
| nodejs/node | `coverage-linux-without-intl.yml` | pass | — |
| nodejs/node | `coverage-linux.yml` | pass | — |
| nodejs/node | `create-release-proposal.yml` | **fail** | `actions/checkout` — step exited with status 1 · `Error: The process '/usr/bin/git' failed with exit code 1` |
| nodejs/node | `daily-wpt-fyi.yml` | **timeout (wedged)** | — |
| nodejs/node | `daily.yml` | pass | — |
| nodejs/node | `doc.yml` | pass | — |
| nodejs/node | `find-inactive-collaborators.yml` | expected failure | `gr2m/create-or-update-pull-request-action` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `find-inactive-tsc.yml` | expected failure | `gr2m/create-or-update-pull-request-action` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `label-flaky-test-issue.yml` | pass | — |
| nodejs/node | `label-pr.yml` | expected failure | `nodejs/node-pr-labeler` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `license-builder.yml` | expected failure | `gr2m/create-or-update-pull-request-action` — step exited with status 1 · `Error: Command failed with exit code 128 (Unknown system error -128): git status` _(mutates pull requests over the API)_ |
| nodejs/node | `lint-release-proposal.yml` | pass | — |
| nodejs/node | `linters.yml` | **timeout** | — |
| nodejs/node | `major-release.yml` | pass | — |
| nodejs/node | `nix-changes-comment.yml` | pass | — |
| nodejs/node | `notify-on-push.yml` | pass | — |
| nodejs/node | `notify-on-review-wanted.yml` | pass | — |
| nodejs/node | `post-release.yml` | pass | — |
| nodejs/node | `scorecard.yml` | **timeout** | — |
| nodejs/node | `stale.yml` | pass | — |
| nodejs/node | `test-internet.yml` | pass | — |
| nodejs/node | `test-linux-quic.yml` | not lowered | `action.local_missing` |
| nodejs/node | `test-linux.yml` | not lowered | `action.local_missing` |
| nodejs/node | `test-shared.yml` | not lowered | `runs_on.expression` |
| nodejs/node | `timezone-update.yml` | pass | — |
| nodejs/node | `tools.yml` | expected failure | `peter-evans/create-pull-request` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `update-openssl.yml` | pass | — |
| nodejs/node | `update-v8.yml` | expected failure | `peter-evans/create-pull-request` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `update-wpt.yml` | expected failure | `gr2m/create-or-update-pull-request-action` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| ohmyzsh/ohmyzsh | `dependencies.yml` | expected failure | `actions/create-github-app-token` — no secret named `OHMYZSH_CLIENT_ID` _(needs a repository secret)_ |
| ohmyzsh/ohmyzsh | `main.yml` | pass | — |
| ohmyzsh/ohmyzsh | `project.yml` | expected failure | `actions/create-github-app-token` — no secret named `OHMYZSH_CLIENT_ID` _(needs a repository secret)_ |
| ohmyzsh/ohmyzsh | `scorecard.yml` | **fail** | `ghcr.io/ossf/scorecard-action:v2.4.4` — step exited with status 1 · `{"date":"2026-08-28T22:18:30Z","repo":{"name":"github.com/ohmyzsh/ohmyzsh","commit":"146461f7c6d95f4ba1220559d66eb113418b40a8"},"scorecard":{"version":"v5.5.0","commit":"c395761df6afe1a69e476bc60a013a…` |
| pola-rs/polars | `benchmark-remote.yml` | not lowered | `runs_on.unknown` |
| pola-rs/polars | `benchmark.yml` | **timeout** | — |
| pola-rs/polars | `changes-dsl-labeler.yml` | pass | — |
| pola-rs/polars | `clear-caches.yml` | pass | — |
| pola-rs/polars | `docs-python.yml` | **fail** | `actions/checkout` — step exited with status 1 · `Error: The process '/usr/bin/git' failed with exit code 1` |
| pola-rs/polars | `docs-rust.yml` | expected failure | `JamesIves/github-pages-deploy-action` — step exited with status 1 · `Notice: Deployment failed! ❌` _(pushes a deployment branch)_ |
| pola-rs/polars | `issue-labeler.yml` | expected failure | `github/issue-labeler` — step exited with status 1 · `Error: HttpError: Not Found` _(mutates issues over the API)_ |
| pola-rs/polars | `lint-global.yml` | pass | — |
| pola-rs/polars | `lint-python.yml` | pass | — |
| pola-rs/polars | `lint-rust.yml` | pass | — |
| pola-rs/polars | `pr-labeler.yml` | expected failure | `release-drafter/release-drafter` — step exited with status 1 · `Error: Invalid config file` _(mutates releases over the API)_ |
| pola-rs/polars | `release-drafter.yml` | expected failure | `release-drafter/release-drafter` — step exited with status 1 · `Error: Invalid config file` _(mutates releases over the API)_ |
| pola-rs/polars | `release-rust.yml` | pass | — |
| pola-rs/polars | `test-bytecode-parser.yml` | pass | — |
| pola-rs/polars | `test-pyodide.yml` | **timeout** | — |
| prometheus/prometheus | `approve-workflows.yml` | pass | — |
| prometheus/prometheus | `automerge-dependabot.yml` | pass | — |
| prometheus/prometheus | `buf-lint.yml` | **fail** | `bufbuild/buf-lint-action` — step exited with status 1 · `Failure: Module "path: "prompb"" had no .proto files` |
| prometheus/prometheus | `buf.yml` | **fail** | `bufbuild/buf-lint-action` — step exited with status 1 · `Error: Failure: Module "path: "prompb"" had no .proto files ` |
| prometheus/prometheus | `check_release_notes.yml` | pass | — |
| prometheus/prometheus | `codeql-analysis.yml` | **timeout** | — |
| prometheus/prometheus | `container_description.yml` | expected failure | `docker.io/chko/docker-pushrm:1` — no secret named `DOCKER_HUB_PASSWORD` _(needs a repository secret)_ |
| prometheus/prometheus | `fuzzing.yml` | pass | — |
| prometheus/prometheus | `govulncheck.yml` | pass | — |
| prometheus/prometheus | `lock.yml` | expected failure | `dessant/lock-threads` — no secret named `PROMBOT_LOCKTHREADS_TOKEN` _(needs a repository secret)_ |
| prometheus/prometheus | `prombench.yml` | pass | — |
| prometheus/prometheus | `repo_sync.yml` | **fail** | `github/checkout` — step exited with status 1 · `Error response from daemon: container fc58d81bcc3944393064e9ce3d7104468cd1be124a46d3f136099d948a30ab15 is not running` |
| prometheus/prometheus | `scorecards.yml` | **fail** | `ghcr.io/ossf/scorecard-action:v2.4.4` — step exited with status 1 · `{"date":"2026-08-28T22:25:05Z","repo":{"name":"github.com/prometheus/prometheus","commit":"342884f747d6db5b79fc46a96b9892bea9ac7103"},"scorecard":{"version":"v5.5.0","commit":"c395761df6afe1a69e476bc6…` |
| prometheus/prometheus | `stale.yml` | pass | — |
| python/cpython | `add-issue-header.yml` | **fail** | `actions/github-script` — step exited with status 1 · `Error: Unhandled error: TypeError: issue_data.labels is not iterable` |
| python/cpython | `lint.yml` | **fail** | `j178/prek-action` — step exited with status 1 · `Error: prek exited with code 2` |
| python/cpython | `mypy.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 · `Error: ENOENT: no such file or directory, scandir 'null'` |
| python/cpython | `new-bugs-announce-notifier.yml` | expected failure | `actions/github-script` — no secret named `MAILGUN_PYTHON_ORG_MAILGUN_KEY` _(needs a repository secret)_ |
| python/cpython | `require-pr-label.yml` | expected failure | `mheap/github-action-required-labels` — step exited with status 1 · `Error: Not Found` _(reads the triggering pull request (no local event))_ |
| python/cpython | `reusable-check-c-api-docs.yml` | pass | — |
| python/cpython | `reusable-check-html-ids.yml` | **fail** | `actions/checkout` — step exited with status 1 · `Error: The process '/usr/bin/git' failed with exit code 1` |
| python/cpython | `reusable-cifuzz.yml` | **fail** | `Dockerfile` — unsafe action path `../../../build_fuzzers.Dockerfile`: every path component must be a normal name |
| python/cpython | `reusable-context.yml` | **timeout** | — |
| python/cpython | `reusable-docs.yml` | **timeout** | — |
| python/cpython | `reusable-emscripten.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-install.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-san.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-wasi.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `stale.yml` | pass | — |
| python/cpython | `verify-ensurepip-wheels.yml` | pass | — |
| python/cpython | `verify-expat.yml` | pass | — |
| rails/rails | `check-markdown-api.yml` | pass | — |
| rails/rails | `devcontainer-shellcheck.yml` | pass | — |
| rails/rails | `devcontainer-smoke-test.yml` | **fail** | `docker/login-action` — step exited with status 1 · `Error: Unable to locate executable file: docker. Please verify either the file path exists or the file can be found within a directory specified by the PATH environment variable. Also check the file m…` |
| rails/rails | `labeler.yml` | pass | — |
| rails/rails | `more-info-needed.yml` | pass | — |
| rails/rails | `rail_inspector.yml` | pass | — |
| rails/rails | `rails-new-docker.yml` | pass | — |
| rails/rails | `rails_releaser_tests.yml` | pass | — |
| rails/rails | `release.yml` | expected failure | `rubygems/configure-rubygems-credentials` — step exited with status 1 · `Error: Error message: Unable to get ACTIONS_ID_TOKEN_REQUEST_URL env variable` _(OIDC token exchange)_ |
| rails/rails | `stale.yml` | pass | — |
| rust-lang/cargo | `audit.yml` | not lowered | `continue_on_error.expression` |
| rust-lang/cargo | `contrib.yml` | **fail** | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: /artifact.tar. No artifacts will be uploaded.` |
| rust-lang/cargo | `release.yml` | pass | — |
| sharkdp/bat | `require-changelog-for-PRs.yml` | pass | — |
| tokio-rs/tokio | `audit.yml` | **fail** | `Dockerfile` — step exited with status 126 · `[FATAL tini (7)] exec /entrypoint.sh failed: Permission denied` |
| tokio-rs/tokio | `labeler.yml` | pass | — |
| tokio-rs/tokio | `loom.yml` | pass | — |
| tokio-rs/tokio | `pr-audit.yml` | **fail** | `Dockerfile` — step exited with status 126 · `[FATAL tini (7)] exec /entrypoint.sh failed: Permission denied` |
| tokio-rs/tokio | `stress-test.yml` | pass | — |
| tokio-rs/tokio | `uring-kernel-version-test.yml` | pass | — |
| vercel/next.js | `automated_code_review.yml` | not lowered | — |
| vercel/next.js | `code_freeze.yml` | pass | — |
| vercel/next.js | `create_release_branch.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `issue_lock.yml` | expected failure | `dessant/lock-threads` — step exited with status 1 · `Error: Must have admin rights to Repository.` _(mutates issues over the API)_ |
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
| vercel/next.js | `sync_backport_canary_release.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `test_e2e_project_reset_cron.yml` | expected failure | `actions/setup-node` — no secret named `VERCEL_ADAPTER_TEST_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `test_examples.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: The specified node version file at: /workspace/repo/.node-version does not exist` |
| vercel/next.js | `triage.yml` | pass | — |
| vercel/next.js | `trigger_release.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `turbopack-update-tests-manifest.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_fonts_data.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_react.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_react_poller.yml` | pass | — |
| vercel/next.js | `upload-tests-manifest.yml` | **fail** | `actions/setup-node` — step exited with status 1 · `Error: The specified node version file at: /workspace/repo/.node-version does not exist` |
| vercel/next.js | `upload_preview_tarballs.yml` | pass | — |
