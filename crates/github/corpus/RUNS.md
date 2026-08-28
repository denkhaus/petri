# Corpus run sweep

The run-time counterpart of REPORT.md: every in-scope workflow that lowers, run end to end with `run:` scripts stubbed to `true` and `uses:` steps real. The outcome measures the runtime tier — checkout, action staging and execution, artifact and cache backends, cross-job flow — not the corpus projects' own builds.

Sweep configuration: host scopes rewritten to the pinned runner images `ghcr.io/lithoscomputer/ubuntu-24.04:slim-66c538cd3ef8` (22.04/26.04 variants by label), matrices capped to their first leg, each workflow capped at 300s wall clock, parallelism 6. Identity: the fixed lowering identity (`github.sha` all zeros), a dummy `GITHUB_TOKEN` — token-less by policy.

249 workflows in scope; 240 lower and were run.

| Result (of the 240 run) | Count | Share |
|---|---|---|
| passed | 45 | 19% |
| **failed on a runtime-tier gap** | 170 | 71% |
| expected failure (server-coupled) | 25 | 10% |
| **timed out** | 0 | 0% |

## First failures — gaps, ranked

| First failing step · class | Workflows |
|---|---|
| `actions/checkout · exit_status:1` | 125 |
| `actions/stale · exit_status:1` | 7 |
| `actions/github-script · exit_status:1` | 5 |
| `noop · env_acquire` | 5 |
| `step-security/harden-runner · exit_status:1` | 5 |
| `noop` | 4 |
| `actions/labeler · exit_status:1` | 2 |
| `actions/setup-python · exit_status:1` | 2 |
| `astral-sh/setup-uv · exit_status:1` | 2 |
| `dessant/lock-threads · exit_status:1` | 2 |
| `github/gh-aw-actions/setup · exit_status:1` | 2 |
| `release-drafter/release-drafter · exit_status:1` | 2 |
| `Dockerfile · action_image` | 1 |
| `actions/cache/restore · bad_config` | 1 |
| `denoland/setup-deno · exit_status:1` | 1 |
| `dorny/paths-filter · exit_status:1` | 1 |
| `github/issue-labeler · exit_status:1` | 1 |
| `mheap/github-action-required-labels · exit_status:1` | 1 |
| `run: · env_acquire` | 1 |

## Expected failures — server-coupled, ranked

First failures no local runner can fix: OIDC, GitHub App and repository secrets, third-party SaaS backends, cross-run artifact reads. Kept out of the gap ranking so it never drowns in server-bound noise.

| First failing step · why | Workflows |
|---|---|
| `actions/checkout · needs a repository secret` | 8 |
| `actions/create-github-app-token · needs a repository secret` | 6 |
| `actions/github-script · needs a repository secret` | 3 |
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
| actions/checkout | `check-dist.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| actions/checkout | `codeql-analysis.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| actions/checkout | `licensed.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| actions/checkout | `publish-immutable-actions.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| actions/checkout | `update-main-version.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| actions/checkout | `update-test-ubuntu-git.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `build-docker.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `build-wasm.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `daily_fuzz.yaml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `memory_report.yaml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `notify-dependents.yml` | expected failure | `actions/github-script` — no secret named `RUFF_PRE_COMMIT_PAT` _(needs a repository secret)_ |
| astral-sh/ruff | `publish-crates.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `publish-docs.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `publish-mirror.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/ruff | `publish-playground.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `publish-pypi.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 |
| astral-sh/ruff | `publish-ty-playground.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `publish-versions.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/ruff | `publish-wasm.yml` | expected failure | `actions/download-artifact` — step exited with status 1 _(cross-run artifact download (REST API))_ |
| astral-sh/ruff | `ty-ecosystem-analyzer.yaml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `ty-ecosystem-report.yaml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/ruff | `typing_conformance.yaml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `bench.yml` | not lowered | `runs_on.unknown` |
| astral-sh/uv | `build-docker.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `check-docs.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `check-fmt.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `check-generated-files.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `check-lock.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `check-publish.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `check-release.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `check-zizmor.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `diagnose-workflow-failure.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `fix-bug.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `issue-triage.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `plan.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `promote-pull-request.yml` | pass | — |
| astral-sh/uv | `publish-crates.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `publish-docs.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `publish-mirror.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/uv | `publish-pypi.yml` | **fail** | `astral-sh/setup-uv` — step exited with status 1 |
| astral-sh/uv | `publish-versions.yml` | **fail** | `noop` — could not build the firing environment |
| astral-sh/uv | `pull-request-conflicts.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `pull-request-labels.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `pull-request-security-review.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `rebase-conflicted-pull-request.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `release-prepare.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `reproduce-bug.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `sync-python-releases.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `sync-uv-dev.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `sync-uv-security.yml` | expected failure | `open-security-tools/ost-simple-sts` — no secret named `STS_API_URL` _(needs a repository secret)_ |
| astral-sh/uv | `test-ecosystem.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `update-issue-context.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| astral-sh/uv | `update-pull-request-parent.yml` | pass | — |
| cli/cli | `agentics-maintenance.yml` | **fail** | `actions/cache/restore` — step config is invalid: unsafe action path `../dist/restore-only/index.js`: every path component must be a normal name |
| cli/cli | `bump-go.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| cli/cli | `codeql.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| cli/cli | `copilot-setup-steps.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| cli/cli | `dependabot-triage.lock.yml` | **fail** | `github/gh-aw-actions/setup` — step exited with status 1 |
| cli/cli | `govulncheck.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| cli/cli | `issue-triage.lock.yml` | **fail** | `github/gh-aw-actions/setup` — step exited with status 1 |
| cli/cli | `lint.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| cli/cli | `triage-issues.yml` | pass | — |
| cli/cli | `triage-pull-requests.yml` | pass | — |
| cli/cli | `triage-scheduled-tasks.yml` | pass | — |
| denoland/deno | `cargo_publish.generated.yml` | expected failure | `actions/checkout` — no secret named `DENOBOT_PAT` _(needs a repository secret)_ |
| denoland/deno | `create_prerelease_tag.generated.yml` | expected failure | `actions/checkout` — no secret named `DENOBOT_PAT` _(needs a repository secret)_ |
| denoland/deno | `post_publish.generated.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| denoland/deno | `pr.generated.yml` | **fail** | `denoland/setup-deno` — step exited with status 1 |
| denoland/deno | `start_release.generated.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| denoland/deno | `version_bump.generated.yml` | expected failure | `actions/checkout` — no secret named `DENOBOT_PAT` _(needs a repository secret)_ |
| django/django | `benchmark.yml` | pass | — |
| django/django | `check-migrations.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| django/django | `check_commit_messages.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| django/django | `check_pr_quality.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| django/django | `coverage_comment.yml` | pass | — |
| django/django | `coverage_tests.yml` | pass | — |
| django/django | `docs.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| django/django | `labels.yml` | **fail** | `actions/github-script` — step exited with status 1 |
| django/django | `linters.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| django/django | `new_contributor_pr.yml` | pass | — |
| django/django | `playwright.yml` | pass | — |
| django/django | `postgis.yml` | pass | — |
| django/django | `python_matrix.yml` | pass | — |
| django/django | `schedules.yml` | expected failure | `actions/github-script` — no secret named `SCHEDULE_WORKFLOW_TOKEN` _(needs a repository secret)_ |
| django/django | `screenshots.yml` | pass | — |
| facebook/react | `compiler_discord_notify.yml` | pass | — |
| facebook/react | `compiler_playground.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| facebook/react | `compiler_prereleases.yml` | expected failure | `actions/checkout` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_prereleases_manual.yml` | expected failure | `actions/checkout` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_prereleases_nightly.yml` | expected failure | `actions/checkout` — no secret named `NPM_TOKEN` _(needs a repository secret)_ |
| facebook/react | `compiler_rust.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| facebook/react | `compiler_typescript.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| facebook/react | `devtools_discord_notify.yml` | pass | — |
| facebook/react | `devtools_regression_tests.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| facebook/react | `runtime_build_and_test.yml` | not lowered | `step.background` |
| facebook/react | `runtime_commit_artifacts.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| facebook/react | `runtime_discord_notify.yml` | pass | — |
| facebook/react | `runtime_eslint_plugin_e2e.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| facebook/react | `runtime_fuzz_tests.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| facebook/react | `runtime_release_from_ci.yml` | expected failure | `tsickert/discord-webhook` — no secret named `DISCORD_WEBHOOK_URL` _(needs a repository secret)_ |
| facebook/react | `runtime_sizebot_comment.yml` | pass | — |
| facebook/react | `shared_check_maintainer.yml` | **fail** | `actions/github-script` — step exited with status 1 |
| facebook/react | `shared_cleanup_merged_branch_caches.yml` | pass | — |
| facebook/react | `shared_cleanup_stale_branch_caches.yml` | pass | — |
| facebook/react | `shared_close_direct_sync_branch_prs.yml` | **fail** | `actions/github-script` — step exited with status 1 |
| facebook/react | `shared_label_core_team_prs.yml` | pass | — |
| facebook/react | `shared_lint.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| facebook/react | `shared_stale.yml` | **fail** | `actions/stale` — step exited with status 1 |
| hashicorp/terraform | `backport.yml` | **fail** | `run:` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| hashicorp/terraform | `changelog-validation.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| hashicorp/terraform | `checks.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| hashicorp/terraform | `enforce-changelog.yml` | **fail** | `dorny/paths-filter` — step exited with status 1 |
| hashicorp/terraform | `equivalence-test-diff.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| hashicorp/terraform | `equivalence-test-manual-update.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| hashicorp/terraform | `equivalence-test-update.yml` | pass | — |
| hashicorp/terraform | `issue-comment-created.yml` | pass | — |
| hashicorp/terraform | `lock.yml` | **fail** | `dessant/lock-threads` — step exited with status 1 |
| nodejs/node | `auto-start-ci.yml` | pass | — |
| nodejs/node | `build-tarball.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `close-stalled.yml` | **fail** | `actions/stale` — step exited with status 1 |
| nodejs/node | `codeql.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `comment-labeled.yml` | pass | — |
| nodejs/node | `commit-lint.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `commit-queue.yml` | pass | — |
| nodejs/node | `coverage-linux-without-intl.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `coverage-linux.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `create-release-proposal.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `daily-wpt-fyi.yml` | **fail** | `actions/setup-python` — step exited with status 1 |
| nodejs/node | `daily.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `doc.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `find-inactive-collaborators.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `find-inactive-tsc.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `label-flaky-test-issue.yml` | pass | — |
| nodejs/node | `label-pr.yml` | expected failure | `nodejs/node-pr-labeler` — no secret named `GH_USER_TOKEN` _(needs a repository secret)_ |
| nodejs/node | `license-builder.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `lint-release-proposal.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `linters.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `major-release.yml` | pass | — |
| nodejs/node | `nix-changes-comment.yml` | pass | — |
| nodejs/node | `notify-on-push.yml` | pass | — |
| nodejs/node | `notify-on-review-wanted.yml` | pass | — |
| nodejs/node | `post-release.yml` | pass | — |
| nodejs/node | `scorecard.yml` | **fail** | `step-security/harden-runner` — step exited with status 1 |
| nodejs/node | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 |
| nodejs/node | `test-internet.yml` | pass | — |
| nodejs/node | `test-linux-quic.yml` | not lowered | `action.local_missing` |
| nodejs/node | `test-linux.yml` | not lowered | `action.local_missing` |
| nodejs/node | `test-shared.yml` | not lowered | `runs_on.expression` |
| nodejs/node | `timezone-update.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `tools.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `update-openssl.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `update-v8.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| nodejs/node | `update-wpt.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| ohmyzsh/ohmyzsh | `dependencies.yml` | **fail** | `step-security/harden-runner` — step exited with status 1 |
| ohmyzsh/ohmyzsh | `main.yml` | **fail** | `step-security/harden-runner` — step exited with status 1 |
| ohmyzsh/ohmyzsh | `project.yml` | **fail** | `step-security/harden-runner` — step exited with status 1 |
| ohmyzsh/ohmyzsh | `scorecard.yml` | **fail** | `step-security/harden-runner` — step exited with status 1 |
| pola-rs/polars | `benchmark-remote.yml` | not lowered | `runs_on.unknown` |
| pola-rs/polars | `benchmark.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| pola-rs/polars | `changes-dsl-labeler.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| pola-rs/polars | `clear-caches.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| pola-rs/polars | `docs-python.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| pola-rs/polars | `docs-rust.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| pola-rs/polars | `issue-labeler.yml` | **fail** | `github/issue-labeler` — step exited with status 1 |
| pola-rs/polars | `lint-global.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| pola-rs/polars | `lint-python.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| pola-rs/polars | `lint-rust.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| pola-rs/polars | `pr-labeler.yml` | **fail** | `release-drafter/release-drafter` — step exited with status 1 |
| pola-rs/polars | `release-drafter.yml` | **fail** | `release-drafter/release-drafter` — step exited with status 1 |
| pola-rs/polars | `release-rust.yml` | pass | — |
| pola-rs/polars | `test-bytecode-parser.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| pola-rs/polars | `test-pyodide.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| prometheus/prometheus | `approve-workflows.yml` | pass | — |
| prometheus/prometheus | `automerge-dependabot.yml` | pass | — |
| prometheus/prometheus | `buf-lint.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| prometheus/prometheus | `buf.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| prometheus/prometheus | `check_release_notes.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| prometheus/prometheus | `codeql-analysis.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| prometheus/prometheus | `container_description.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| prometheus/prometheus | `fuzzing.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| prometheus/prometheus | `govulncheck.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| prometheus/prometheus | `lock.yml` | expected failure | `dessant/lock-threads` — no secret named `PROMBOT_LOCKTHREADS_TOKEN` _(needs a repository secret)_ |
| prometheus/prometheus | `prombench.yml` | pass | — |
| prometheus/prometheus | `repo_sync.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| prometheus/prometheus | `scorecards.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| prometheus/prometheus | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 |
| python/cpython | `add-issue-header.yml` | **fail** | `actions/github-script` — step exited with status 1 |
| python/cpython | `lint.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| python/cpython | `mypy.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| python/cpython | `new-bugs-announce-notifier.yml` | expected failure | `actions/github-script` — no secret named `MAILGUN_PYTHON_ORG_MAILGUN_KEY` _(needs a repository secret)_ |
| python/cpython | `require-pr-label.yml` | **fail** | `mheap/github-action-required-labels` — step exited with status 1 |
| python/cpython | `reusable-check-c-api-docs.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| python/cpython | `reusable-check-html-ids.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| python/cpython | `reusable-cifuzz.yml` | **fail** | `Dockerfile` — unsafe action path `../../../build_fuzzers.Dockerfile`: every path component must be a normal name |
| python/cpython | `reusable-context.yml` | **fail** | `actions/setup-python` — step exited with status 1 |
| python/cpython | `reusable-docs.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-emscripten.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-install.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-san.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `reusable-wasi.yml` | **fail** | `noop` — could not acquire the environment: docker pull failed: no matching manifest for linux/arm64/v8 in the manifest list entries |
| python/cpython | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 |
| python/cpython | `verify-ensurepip-wheels.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| python/cpython | `verify-expat.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| rails/rails | `check-markdown-api.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| rails/rails | `devcontainer-shellcheck.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| rails/rails | `devcontainer-smoke-test.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| rails/rails | `labeler.yml` | **fail** | `actions/labeler` — step exited with status 1 |
| rails/rails | `more-info-needed.yml` | **fail** | `actions/stale` — step exited with status 1 |
| rails/rails | `rail_inspector.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| rails/rails | `rails-new-docker.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| rails/rails | `rails_releaser_tests.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| rails/rails | `release.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| rails/rails | `stale.yml` | **fail** | `actions/stale` — step exited with status 1 |
| rust-lang/cargo | `audit.yml` | not lowered | `continue_on_error.expression` |
| rust-lang/cargo | `contrib.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| rust-lang/cargo | `release.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| sharkdp/bat | `require-changelog-for-PRs.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| tokio-rs/tokio | `audit.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| tokio-rs/tokio | `labeler.yml` | **fail** | `actions/labeler` — step exited with status 1 |
| tokio-rs/tokio | `loom.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| tokio-rs/tokio | `pr-audit.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| tokio-rs/tokio | `stress-test.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| tokio-rs/tokio | `uring-kernel-version-test.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| vercel/next.js | `automated_code_review.yml` | not lowered | — |
| vercel/next.js | `code_freeze.yml` | pass | — |
| vercel/next.js | `create_release_branch.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
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
| vercel/next.js | `sync_backport_canary_release.yml` | **fail** | `actions/github-script` — step exited with status 1 |
| vercel/next.js | `test_e2e_project_reset_cron.yml` | expected failure | `actions/checkout` — no secret named `VERCEL_ADAPTER_TEST_TOKEN` _(needs a repository secret)_ |
| vercel/next.js | `test_examples.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| vercel/next.js | `triage.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| vercel/next.js | `trigger_release.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `turbopack-update-tests-manifest.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_fonts_data.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_react.yml` | expected failure | `actions/create-github-app-token` — no secret named `RELEASE_GITHUB_APP_PRIVATE_KEY` _(needs a repository secret)_ |
| vercel/next.js | `update_react_poller.yml` | pass | — |
| vercel/next.js | `upload-tests-manifest.yml` | **fail** | `actions/checkout` — step exited with status 1 |
| vercel/next.js | `upload_preview_tarballs.yml` | pass | — |
