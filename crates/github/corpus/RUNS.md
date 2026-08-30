# Corpus run sweep

The run-time counterpart of REPORT.md: every in-scope workflow that lowers, run end to end with `run:` scripts stubbed to `true` and `uses:` steps real. The outcome measures the runtime tier — checkout, action staging and execution, artifact and cache backends, cross-job flow — not the corpus projects' own builds.

Sweep configuration: host scopes rewritten to the pinned runner images `ghcr.io/lithoscomputer/ubuntu-24.04:slim-e38f48b4bcd5` (22.04/26.04 variants by label; the privileged dind flavor where the graph drives a Docker engine), matrices capped to their first leg, each workflow capped at 900s wall clock, parallelism 4; the full-image list runs on the ubuntu-latest capture, since its workflows compile native gems against full-image packages. The artifact and cache backends are live: each run gets the distribution's ObjectService, cache and tool cache in the host store, persistent across sweeps. Identity: `github.sha` is the repo's pinned corpus commit (`corpus-pins.txt`) — the sweep's analog of `default_params` reading HEAD — so `checkout` fetches real state; a real `GITHUB_TOKEN` (`PETRI_SWEEP_TOKEN`) authenticated actions' API calls, so no anonymous rate limit applied.

248 workflows in scope; 241 lower and were run.

| Result (of the 241 run) | Count | Share |
|---|---|---|
| passed | 146 | 61% |
| **failed on a runtime-tier gap** | 7 | 3% |
| expected failure (server-coupled) | 87 | 36% |
| **timed out** | 1 | 0% |

## First failures — gaps, ranked

| First failing step · class | Workflows |
|---|---|
| `actions/setup-python · exit_status:1` | 2 |
| `Dockerfile · exit_status:1` | 1 |
| `PyO3/maturin-action · exit_status:1` | 1 |
| `devcontainers/ci · exit_status:1` | 1 |
| `github/checkout · exit_status:1` | 1 |
| `liskin/gh-problem-matcher-wrap · exit_status:1` | 1 |

## Expected failures — server-coupled, ranked

First failures the sweep's own stance produces or no token-less local run can fix: OIDC, GitHub App and repository secrets, git credentials for the real checkout action, third-party SaaS backends, cross-run artifact reads, steps that read an output a stubbed `run:` script would have written, refs and event payloads only a real triggering run has, and actions that read or write only the hosted service can answer. Kept out of the gap ranking so it never drowns in server-bound noise.

| First failing step · why | Workflows |
|---|---|
| `actions/create-github-app-token · needs a repository secret` | 9 |
| `actions/upload-artifact · uploads outputs a stubbed build never produced` | 6 |
| `actions/github-script · needs a repository secret` | 5 |
| `actions/setup-node · needs a repository secret` | 5 |
| `open-security-tools/ost-simple-sts · needs a repository secret` | 5 |
| `actions/download-artifact · cross-run artifact download (REST API)` | 4 |
| `github/codeql-action/init · the toolchain ships no build for this architecture; a host of a supported architecture runs it` | 4 |
| `job marker · requires its caller's inputs (a reusable workflow run standalone)` | 4 |
| `actions/checkout · needs a repository secret` | 3 |
| `actions/github-script · the inline script threw (it acts on the triggering event, which a local run lacks)` | 3 |
| `actions/setup-go · reads a stubbed script's output` | 3 |
| `ghcr.io/ossf/scorecard-action:v2.4.4 · signs results with the Actions-issued GITHUB_TOKEN` | 3 |
| `gr2m/create-or-update-pull-request-action · needs a repository secret` | 3 |
| `JamesIves/github-pages-deploy-action · pushes a deployment branch` | 2 |
| `actions/checkout · checks out a ref built from an empty dispatch input` | 2 |
| `dessant/lock-threads · mutates issues over the API` | 2 |
| `peter-evans/create-pull-request · needs a repository secret` | 2 |
| `release-drafter/release-drafter · mutates releases over the API` | 2 |
| `rust-lang/crates-io-auth-action · OIDC token exchange` | 2 |
| `./.github/actions/upload-turboyet-data · needs a repository secret` | 1 |
| `actions/cache · reads a stubbed script's output` | 1 |
| `actions/publish-immutable-action · publishes to GitHub's registry` | 1 |
| `actions/stale · needs a repository secret` | 1 |
| `actions/upload-artifact · reads a stubbed script's output` | 1 |
| `depot/build-push-action · builds on the depot.dev service (project token)` | 1 |
| `dessant/lock-threads · needs a repository secret` | 1 |
| `docker.io/chko/docker-pushrm:1 · needs a repository secret` | 1 |
| `dorny/paths-filter · reads the triggering pull request (no local event)` | 1 |
| `github/codeql-action/upload-sarif · uploads to GitHub code scanning` | 1 |
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
| actions/checkout | `codeql-analysis.yml` | expected failure | `github/codeql-action/init` — step exited with status 1 · `Error: Unable to download and extract CodeQL CLI: The CodeQL CLI does not support the platform/architecture combination of linux/arm64 (see https://codeql.github.com/docs/codeql-overview/system-requir…` _(the toolchain ships no build for this architecture; a host of a supported architecture runs it)_ |
| actions/checkout | `licensed.yml` | pass | — |
| actions/checkout | `publish-immutable-actions.yml` | expected failure | `actions/publish-immutable-action` — step exited with status 1 · `Error: Could not find Repository.` _(publishes to GitHub's registry)_ |
| actions/checkout | `update-main-version.yml` | pass | — |
| actions/checkout | `update-test-ubuntu-git.yml` | pass | — |
| astral-sh/ruff | `build-docker.yml` | **timeout** | — |
| astral-sh/ruff | `build-wasm.yml` | pass | — |
| astral-sh/ruff | `daily_fuzz.yaml` | pass | — |
| astral-sh/ruff | `memory_report.yaml` | pass | — |
| astral-sh/ruff | `notify-dependents.yml` | expected failure | `actions/github-script` — no secret named `RUFF_PRE_COMMIT_PAT` _(needs a repository secret)_ |
| astral-sh/ruff | `publish-crates.yml` | expected failure | `rust-lang/crates-io-auth-action` — step exited with status 1 · `Error: Please ensure the 'id-token' permission is set to 'write' in your workflow. For more information, see: https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-dep…` _(OIDC token exchange)_ |
| astral-sh/ruff | `publish-docs.yml` | pass | — |
| astral-sh/ruff | `publish-mirror.yml` | expected failure | `job marker` — could not build the firing environment: node NodeId(0): evaluating scope env `VERSION` failed: from_json needs valid JSON, got invalid JSON (EOF while parsing a value at line 1 column 0) _(requires its caller's inputs (a reusable workflow run standalone))_ |
| astral-sh/ruff | `publish-playground.yml` | expected failure | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: playground/ruff/dist. No artifacts will be uploaded.` _(uploads outputs a stubbed build never produced)_ |
| astral-sh/ruff | `publish-pypi.yml` | pass | — |
| astral-sh/ruff | `publish-ty-playground.yml` | expected failure | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: playground/ty/dist. No artifacts will be uploaded.` _(uploads outputs a stubbed build never produced)_ |
| astral-sh/ruff | `publish-versions.yml` | expected failure | `job marker` — could not build the firing environment: node NodeId(0): evaluating scope env `VERSION` failed: from_json needs valid JSON, got invalid JSON (EOF while parsing a value at line 1 column 0) _(requires its caller's inputs (a reusable workflow run standalone))_ |
| astral-sh/ruff | `publish-wasm.yml` | expected failure | `actions/download-artifact` — step exited with status 1 · `Error: Unable to download artifact(s): Artifact not found for name: wasm-npm-web         Please ensure that your artifact is not expired and the artifact was uploaded using a compatible version of too…` _(cross-run artifact download (REST API))_ |
| astral-sh/ruff | `ty-ecosystem-analyzer.yaml` | expected failure | `actions/download-artifact` — step exited with status 1 · `Error: Unable to download artifact(s): Artifact not found for name: ty-build-dependencies         Please ensure that your artifact is not expired and the artifact was uploaded using a compatible versi…` _(cross-run artifact download (REST API))_ |
| astral-sh/ruff | `ty-ecosystem-report.yaml` | pass | — |
| astral-sh/ruff | `typing_conformance.yaml` | pass | — |
| astral-sh/uv | `bench.yml` | not lowered | `runs_on.unknown` |
| astral-sh/uv | `build-docker.yml` | expected failure | `depot/build-push-action` — step exited with status 1 · `Error: Input does not meet YAML 1.2 "Core Schema" specification: push Support boolean input list: `true \| True \| TRUE \| false \| False \| FALSE`` _(builds on the depot.dev service (project token))_ |
| astral-sh/uv | `check-docs.yml` | pass | — |
| astral-sh/uv | `check-fmt.yml` | pass | — |
| astral-sh/uv | `check-generated-files.yml` | pass | — |
| astral-sh/uv | `check-lock.yml` | pass | — |
| astral-sh/uv | `check-publish.yml` | pass | — |
| astral-sh/uv | `check-release.yml` | pass | — |
| astral-sh/uv | `check-zizmor.yml` | pass | — |
| astral-sh/uv | `diagnose-workflow-failure.yml` | pass | — |
| astral-sh/uv | `fix-bug.yml` | expected failure | `actions/download-artifact` — step exited with status 1 · `Error: Unable to download artifact(s): Not Found - https://docs.github.com/rest/actions/artifacts#list-workflow-run-artifacts` _(cross-run artifact download (REST API))_ |
| astral-sh/uv | `issue-triage.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `plan.yml` | pass | — |
| astral-sh/uv | `promote-pull-request.yml` | pass | — |
| astral-sh/uv | `publish-crates.yml` | expected failure | `rust-lang/crates-io-auth-action` — step exited with status 1 · `Error: Please ensure the 'id-token' permission is set to 'write' in your workflow. For more information, see: https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-dep…` _(OIDC token exchange)_ |
| astral-sh/uv | `publish-docs.yml` | pass | — |
| astral-sh/uv | `publish-mirror.yml` | expected failure | `job marker` — could not build the firing environment: node NodeId(0): evaluating scope env `VERSION` failed: from_json needs valid JSON, got invalid JSON (EOF while parsing a value at line 1 column 0) _(requires its caller's inputs (a reusable workflow run standalone))_ |
| astral-sh/uv | `publish-pypi.yml` | pass | — |
| astral-sh/uv | `publish-versions.yml` | expected failure | `job marker` — could not build the firing environment: node NodeId(0): evaluating scope env `VERSION` failed: from_json needs valid JSON, got invalid JSON (EOF while parsing a value at line 1 column 0) _(requires its caller's inputs (a reusable workflow run standalone))_ |
| astral-sh/uv | `pull-request-conflicts.yml` | expected failure | `actions/upload-artifact` — step config failed to resolve _(reads a stubbed script's output)_ |
| astral-sh/uv | `pull-request-labels.yml` | expected failure | `actions/checkout` — step exited with status 1 · `Error: The process '/usr/bin/git' failed with exit code 128` _(checks out a ref built from an empty dispatch input)_ |
| astral-sh/uv | `pull-request-security-review.yml` | pass | — |
| astral-sh/uv | `rebase-conflicted-pull-request.yml` | expected failure | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: /workspace/.ci/temp/rebased.bundle. No artifacts will be uploaded.` _(uploads outputs a stubbed build never produced)_ |
| astral-sh/uv | `release-prepare.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `reproduce-bug.yml` | pass | — |
| astral-sh/uv | `sync-python-releases.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `sync-uv-dev.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `sync-uv-security.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `test-ecosystem.yml` | expected failure | `actions/download-artifact` — step exited with status 1 · `Error: Unable to download artifact(s): Artifact not found for name: uv-linux-libc-         Please ensure that your artifact is not expired and the artifact was uploaded using a compatible version of t…` _(cross-run artifact download (REST API))_ |
| astral-sh/uv | `update-issue-context.yml` | pass | — |
| astral-sh/uv | `update-pull-request-parent.yml` | pass | — |
| cli/cli | `agentics-maintenance.yml` | pass | — |
| cli/cli | `bump-go.yml` | pass | — |
| cli/cli | `codeql.yml` | expected failure | `github/codeql-action/init` — step exited with status 1 · `Error: Unable to download and extract CodeQL CLI: The CodeQL CLI does not support the platform/architecture combination of linux/arm64 (see https://codeql.github.com/docs/codeql-overview/system-requir…` _(the toolchain ships no build for this architecture; a host of a supported architecture runs it)_ |
| cli/cli | `copilot-setup-steps.yml` | pass | — |
| cli/cli | `dependabot-triage.lock.yml` | expected failure | `actions/github-script` — no secret named `GH_AW_GITHUB_MCP_SERVER_TOKEN` _(needs a repository secret)_ |
| cli/cli | `govulncheck.yml` | expected failure | `github/codeql-action/upload-sarif` — step exited with status 1 · `Error: Path does not exist: gh.sarif` _(uploads to GitHub code scanning)_ |
| cli/cli | `issue-triage.lock.yml` | expected failure | `actions/github-script` — no secret named `GH_AW_GITHUB_MCP_SERVER_TOKEN` _(needs a repository secret)_ |
| cli/cli | `lint.yml` | pass | — |
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
| django/django | `check-migrations.yml` | pass | — |
| django/django | `check_commit_messages.yml` | pass | — |
| django/django | `check_pr_quality.yml` | pass | — |
| django/django | `coverage_comment.yml` | pass | — |
| django/django | `coverage_tests.yml` | pass | — |
| django/django | `docs.yml` | pass | — |
| django/django | `labels.yml` | expected failure | `actions/github-script` — step exited with status 1 · `Error: Unhandled error: TypeError: Cannot read properties of undefined (reading 'title')` _(the inline script threw (it acts on the triggering event, which a local run lacks))_ |
| django/django | `linters.yml` | **fail** | `liskin/gh-problem-matcher-wrap` — step exited with status 1 · `Error: Unable to locate executable file: flake8. Please verify either the file path exists or the file can be found within a directory specified by the PATH environment variable. Also check the file m…` |
| django/django | `new_contributor_pr.yml` | pass | — |
| django/django | `playwright.yml` | pass | — |
| django/django | `postgis.yml` | pass | — |
| django/django | `python_matrix.yml` | pass | — |
| django/django | `schedules.yml` | expected failure | `actions/github-script` — no secret named `SCHEDULE_WORKFLOW_TOKEN` _(needs a repository secret)_ |
| django/django | `screenshots.yml` | pass | — |
| facebook/react | `compiler_discord_notify.yml` | pass | — |
| facebook/react | `compiler_playground.yml` | pass | — |
| facebook/react | `compiler_prereleases.yml` | expected failure | `actions/setup-node` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_prereleases_manual.yml` | expected failure | `actions/setup-node` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_prereleases_nightly.yml` | expected failure | `actions/setup-node` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_rust.yml` | pass | — |
| facebook/react | `compiler_typescript.yml` | pass | — |
| facebook/react | `devtools_discord_notify.yml` | pass | — |
| facebook/react | `devtools_regression_tests.yml` | expected failure | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: build. No artifacts will be uploaded.` _(uploads outputs a stubbed build never produced)_ |
| facebook/react | `runtime_build_and_test.yml` | not lowered | `step.background` |
| facebook/react | `runtime_commit_artifacts.yml` | expected failure | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: build/. No artifacts will be uploaded.` _(uploads outputs a stubbed build never produced)_ |
| facebook/react | `runtime_discord_notify.yml` | pass | — |
| facebook/react | `runtime_eslint_plugin_e2e.yml` | pass | — |
| facebook/react | `runtime_fuzz_tests.yml` | pass | — |
| facebook/react | `runtime_release_from_ci.yml` | expected failure | `tsickert/discord-webhook` — no secret named `DISCORD_WEBHOOK_URL` _(needs a repository secret)_ |
| facebook/react | `runtime_sizebot_comment.yml` | pass | — |
| facebook/react | `shared_check_maintainer.yml` | pass | — |
| facebook/react | `shared_cleanup_merged_branch_caches.yml` | pass | — |
| facebook/react | `shared_cleanup_stale_branch_caches.yml` | pass | — |
| facebook/react | `shared_close_direct_sync_branch_prs.yml` | expected failure | `actions/github-script` — step exited with status 1 · `Error: Unhandled error: SyntaxError: Unexpected token ';'` _(the inline script threw (it acts on the triggering event, which a local run lacks))_ |
| facebook/react | `shared_label_core_team_prs.yml` | pass | — |
| facebook/react | `shared_lint.yml` | pass | — |
| facebook/react | `shared_stale.yml` | pass | — |
| hashicorp/terraform | `backport.yml` | pass | — |
| hashicorp/terraform | `changelog-validation.yml` | pass | — |
| hashicorp/terraform | `checks.yml` | expected failure | `actions/setup-go` — step exited with status 1 · `Error: Command failed:  version /bin/sh: 1: version: not found ` _(reads a stubbed script's output)_ |
| hashicorp/terraform | `enforce-changelog.yml` | expected failure | `dorny/paths-filter` — step exited with status 1 · `Error: The process 'git rev-parse --abbrev-ref HEAD' failed with exit code 128` _(reads the triggering pull request (no local event))_ |
| hashicorp/terraform | `equivalence-test-diff.yml` | expected failure | `actions/setup-go` — step exited with status 1 · `Error: Command failed:  version /bin/sh: 1: version: not found ` _(reads a stubbed script's output)_ |
| hashicorp/terraform | `equivalence-test-manual-update.yml` | expected failure | `actions/setup-go` — step exited with status 1 · `Error: Command failed:  version /bin/sh: 1: version: not found ` _(reads a stubbed script's output)_ |
| hashicorp/terraform | `equivalence-test-update.yml` | pass | — |
| hashicorp/terraform | `issue-comment-created.yml` | pass | — |
| hashicorp/terraform | `lock.yml` | expected failure | `dessant/lock-threads` — step exited with status 1 · `Error: Must have admin rights to Repository. - https://docs.github.com/rest/issues/issues#lock-an-issue` _(mutates issues over the API)_ |
| nodejs/node | `auto-start-ci.yml` | pass | — |
| nodejs/node | `build-tarball.yml` | pass | — |
| nodejs/node | `close-stalled.yml` | pass | — |
| nodejs/node | `codeql.yml` | expected failure | `github/codeql-action/init` — step exited with status 1 · `Error: Unable to download and extract CodeQL CLI: The CodeQL CLI does not support the platform/architecture combination of linux/arm64 (see https://codeql.github.com/docs/codeql-overview/system-requir…` _(the toolchain ships no build for this architecture; a host of a supported architecture runs it)_ |
| nodejs/node | `comment-labeled.yml` | pass | — |
| nodejs/node | `commit-lint.yml` | pass | — |
| nodejs/node | `commit-queue.yml` | pass | — |
| nodejs/node | `coverage-linux-without-intl.yml` | pass | — |
| nodejs/node | `coverage-linux.yml` | pass | — |
| nodejs/node | `create-release-proposal.yml` | expected failure | `actions/checkout` — step exited with status 1 · `Error: The process '/usr/bin/git' failed with exit code 1` _(checks out a ref built from an empty dispatch input)_ |
| nodejs/node | `daily-wpt-fyi.yml` | pass | — |
| nodejs/node | `daily.yml` | pass | — |
| nodejs/node | `doc.yml` | pass | — |
| nodejs/node | `find-inactive-collaborators.yml` | expected failure | `gr2m/create-or-update-pull-request-action` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `find-inactive-tsc.yml` | expected failure | `gr2m/create-or-update-pull-request-action` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `label-flaky-test-issue.yml` | pass | — |
| nodejs/node | `label-pr.yml` | expected failure | `nodejs/node-pr-labeler` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `license-builder.yml` | expected failure | `gr2m/create-or-update-pull-request-action` — step exited with status 1 · `Error: Command failed with exit code 128 (Unknown system error -128): git push -f https://x-access-token:***@github.com/nodejs/node.git HEAD:refs/heads/actions/license-builder` _(mutates pull requests over the API)_ |
| nodejs/node | `lint-release-proposal.yml` | pass | — |
| nodejs/node | `linters.yml` | pass | — |
| nodejs/node | `major-release.yml` | pass | — |
| nodejs/node | `nix-changes-comment.yml` | pass | — |
| nodejs/node | `notify-on-push.yml` | pass | — |
| nodejs/node | `notify-on-review-wanted.yml` | pass | — |
| nodejs/node | `post-release.yml` | pass | — |
| nodejs/node | `scorecard.yml` | expected failure | `ghcr.io/ossf/scorecard-action:v2.4.4` — step exited with status 1 · `{"date":"2026-08-30T17:50:53Z","repo":{"name":"github.com/nodejs/node","commit":"01c1300dfc168ddce2b44e719bd50eba5ed22015"},"scorecard":{"version":"v5.5.0","commit":"c395761df6afe1a69e476bc60a013a94bc…` _(signs results with the Actions-issued GITHUB_TOKEN)_ |
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
| ohmyzsh/ohmyzsh | `scorecard.yml` | expected failure | `ghcr.io/ossf/scorecard-action:v2.4.4` — step exited with status 1 · `{"date":"2026-08-30T17:53:09Z","repo":{"name":"github.com/ohmyzsh/ohmyzsh","commit":"a5ecff7560b2e26f612032c632a12c75a3048bd0"},"scorecard":{"version":"v5.5.0","commit":"c395761df6afe1a69e476bc60a013a…` _(signs results with the Actions-issued GITHUB_TOKEN)_ |
| pola-rs/polars | `benchmark-remote.yml` | not lowered | `runs_on.unknown` |
| pola-rs/polars | `benchmark.yml` | pass | — |
| pola-rs/polars | `changes-dsl-labeler.yml` | pass | — |
| pola-rs/polars | `clear-caches.yml` | pass | — |
| pola-rs/polars | `docs-python.yml` | expected failure | `JamesIves/github-pages-deploy-action` — step exited with status 1 · `Error: The directory you're trying to deploy named /workspace/repo/py-polars/docs/build/html doesn't exist. Please double check the path and any prerequisite build scripts and try again. ❗` _(pushes a deployment branch)_ |
| pola-rs/polars | `docs-rust.yml` | expected failure | `JamesIves/github-pages-deploy-action` — step exited with status 1 · `Error: The directory you're trying to deploy named /workspace/repo/target/doc doesn't exist. Please double check the path and any prerequisite build scripts and try again. ❗` _(pushes a deployment branch)_ |
| pola-rs/polars | `issue-labeler.yml` | expected failure | `github/issue-labeler` — step exited with status 1 · `Error: Error: Input required and not supplied: issue-number` _(mutates issues over the API)_ |
| pola-rs/polars | `lint-global.yml` | pass | — |
| pola-rs/polars | `lint-python.yml` | pass | — |
| pola-rs/polars | `lint-rust.yml` | pass | — |
| pola-rs/polars | `pr-labeler.yml` | expected failure | `release-drafter/release-drafter` — step exited with status 1 · `Error: Invalid config file` _(mutates releases over the API)_ |
| pola-rs/polars | `release-drafter.yml` | expected failure | `release-drafter/release-drafter` — step exited with status 1 · `Error: Invalid config file` _(mutates releases over the API)_ |
| pola-rs/polars | `release-rust.yml` | pass | — |
| pola-rs/polars | `test-bytecode-parser.yml` | pass | — |
| pola-rs/polars | `test-pyodide.yml` | **fail** | `PyO3/maturin-action` — step exited with status 1 · `Error: Unable to locate executable file: rustup. Please verify either the file path exists or the file can be found within a directory specified by the PATH environment variable. Also check the file m…` |
| prometheus/prometheus | `approve-workflows.yml` | pass | — |
| prometheus/prometheus | `automerge-dependabot.yml` | pass | — |
| prometheus/prometheus | `buf-lint.yml` | pass | — |
| prometheus/prometheus | `buf.yml` | pass | — |
| prometheus/prometheus | `check_release_notes.yml` | pass | — |
| prometheus/prometheus | `codeql-analysis.yml` | expected failure | `github/codeql-action/init` — step exited with status 1 · `Error: Unable to download and extract CodeQL CLI: The CodeQL CLI does not support the platform/architecture combination of linux/arm64 (see https://codeql.github.com/docs/codeql-overview/system-requir…` _(the toolchain ships no build for this architecture; a host of a supported architecture runs it)_ |
| prometheus/prometheus | `container_description.yml` | expected failure | `docker.io/chko/docker-pushrm:1` — no secret named `DOCKER_HUB_PASSWORD` _(needs a repository secret)_ |
| prometheus/prometheus | `fuzzing.yml` | pass | — |
| prometheus/prometheus | `govulncheck.yml` | pass | — |
| prometheus/prometheus | `lock.yml` | expected failure | `dessant/lock-threads` — no secret named `PROMBOT_LOCKTHREADS_TOKEN` _(needs a repository secret)_ |
| prometheus/prometheus | `prombench.yml` | pass | — |
| prometheus/prometheus | `repo_sync.yml` | **fail** | `github/checkout` — step exited with status 1 · `Error response from daemon: container 70d79220ea890f57e4055652dc5dbfbb6e58a8591fce210a28fbc55214adc784 is not running` |
| prometheus/prometheus | `scorecards.yml` | expected failure | `ghcr.io/ossf/scorecard-action:v2.4.4` — step exited with status 1 · `{"date":"2026-08-30T17:57:53Z","repo":{"name":"github.com/prometheus/prometheus","commit":"09fdfcd2659dd9c816e9e23c992fc161c0091757"},"scorecard":{"version":"v5.5.0","commit":"c395761df6afe1a69e476bc6…` _(signs results with the Actions-issued GITHUB_TOKEN)_ |
| prometheus/prometheus | `stale.yml` | pass | — |
| python/cpython | `add-issue-header.yml` | expected failure | `actions/github-script` — step exited with status 1 · `Error: Unhandled error: TypeError: issue_data.labels is not iterable` _(the inline script threw (it acts on the triggering event, which a local run lacks))_ |
| python/cpython | `lint.yml` | pass | — |
| python/cpython | `mypy.yml` | pass | — |
| python/cpython | `new-bugs-announce-notifier.yml` | expected failure | `actions/github-script` — no secret named `MAILGUN_PYTHON_ORG_MAILGUN_KEY` _(needs a repository secret)_ |
| python/cpython | `require-pr-label.yml` | expected failure | `mheap/github-action-required-labels` — step exited with status 1 · `Error: Not Found` _(reads the triggering pull request (no local event))_ |
| python/cpython | `reusable-check-c-api-docs.yml` | pass | — |
| python/cpython | `reusable-check-html-ids.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Error: Unable to locate executable file: lsb_release. Please verify either the file path exists or the file can be found within a directory specified by the PATH environment variable. Also check the f…` |
| python/cpython | `reusable-cifuzz.yml` | **fail** | `Dockerfile` — step exited with status 1 · `config_utils.ConfigError: Invalid Configuration.` |
| python/cpython | `reusable-context.yml` | pass | — |
| python/cpython | `reusable-docs.yml` | **fail** | `actions/setup-python` — step exited with status 1 · `Error: Unable to locate executable file: lsb_release. Please verify either the file path exists or the file can be found within a directory specified by the PATH environment variable. Also check the f…` |
| python/cpython | `reusable-emscripten.yml` | expected failure | `actions/cache` — step exited with status 1 · `Error: Input required and not supplied: path` _(reads a stubbed script's output)_ |
| python/cpython | `reusable-install.yml` | pass | — |
| python/cpython | `reusable-san.yml` | pass | — |
| python/cpython | `reusable-wasi.yml` | pass | — |
| python/cpython | `stale.yml` | pass | — |
| python/cpython | `verify-ensurepip-wheels.yml` | pass | — |
| python/cpython | `verify-expat.yml` | pass | — |
| rails/rails | `check-markdown-api.yml` | pass | — |
| rails/rails | `devcontainer-shellcheck.yml` | pass | — |
| rails/rails | `devcontainer-smoke-test.yml` | **fail** | `devcontainers/ci` — step exited with status 1 · `Error: Dev container config (/workspace/repo/myapp_sqlite/.devcontainer/devcontainer.json) not found.` |
| rails/rails | `labeler.yml` | pass | — |
| rails/rails | `more-info-needed.yml` | pass | — |
| rails/rails | `rail_inspector.yml` | pass | — |
| rails/rails | `rails-new-docker.yml` | pass | — |
| rails/rails | `rails_releaser_tests.yml` | pass | — |
| rails/rails | `release.yml` | expected failure | `rubygems/configure-rubygems-credentials` — step exited with status 1 · `Error: Error message: Unable to get ACTIONS_ID_TOKEN_REQUEST_URL env variable` _(OIDC token exchange)_ |
| rails/rails | `stale.yml` | pass | — |
| rust-lang/cargo | `audit.yml` | pass | — |
| rust-lang/cargo | `contrib.yml` | expected failure | `actions/upload-artifact` — step exited with status 1 · `Error: No files were found with the provided path: /workspace/.ci/temp/artifact.tar. No artifacts will be uploaded.` _(uploads outputs a stubbed build never produced)_ |
| rust-lang/cargo | `release.yml` | pass | — |
| sharkdp/bat | `require-changelog-for-PRs.yml` | pass | — |
| tokio-rs/tokio | `audit.yml` | pass | — |
| tokio-rs/tokio | `labeler.yml` | pass | — |
| tokio-rs/tokio | `loom.yml` | pass | — |
| tokio-rs/tokio | `pr-audit.yml` | pass | — |
| tokio-rs/tokio | `stress-test.yml` | pass | — |
| tokio-rs/tokio | `uring-kernel-version-test.yml` | pass | — |
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
| vercel/next.js | `test_examples.yml` | pass | — |
| vercel/next.js | `triage.yml` | pass | — |
| vercel/next.js | `trigger_release.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `turbopack-update-tests-manifest.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_fonts_data.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_react.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_react_poller.yml` | pass | — |
| vercel/next.js | `upload-tests-manifest.yml` | expected failure | `./.github/actions/upload-turboyet-data` — no secret named `TURBOYET_KV_REST_API_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `upload_preview_tarballs.yml` | pass | — |
