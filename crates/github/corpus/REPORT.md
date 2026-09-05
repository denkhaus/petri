# Compatibility corpus report

316 workflows from 21 repositories. Out of the denominator by policy: 52 need a Windows or macOS runner or shell, 4 are reusable-only files whose runner a caller provides (callee-only), and 1 reference a repository gone upstream (broken on GitHub itself) — leaving **259 in scope**.

Remote `uses:` references resolve through the action snapshot: 305 references, 1 of them unavailable. Refresh with `cargo test -p petri-github-acceptance --test snapshot -- --ignored`.

| Result (of the in-scope 259) | Count | Share |
|---|---|---|
| lowered clean | 24 | 9% |
| lowered with warnings | 224 | 86% |
| rejected with a specific `unsupported.*` code | 11 | 4% |
| **failed for any other reason** | 0 | 0% |
| **panicked** | 0 | 0% |

## Remote actions, by frequency

Which actions the runner meets most. `uses:` references across all workflows; a workflow using an action three times counts three.

| Action | Uses |
|---|---|
| `actions/upload-artifact` | 706 |
| `actions/download-artifact` | 400 |
| `actions/checkout` | 184 |
| `actions/setup-python` | 173 |
| `actions/github-script` | 169 |
| `actions/cache/restore` | 147 |
| `actions/cache` | 139 |
| `Swatinem/rust-cache` | 138 |
| `actions/setup-node` | 128 |
| `actions/cache/save` | 106 |
| `PyO3/maturin-action` | 88 |
| `astral-sh/setup-uv` | 75 |
| `dtolnay/rust-toolchain` | 70 |
| `vercel/setup-turborepo-remote-cache-action` | 66 |
| `taiki-e/install-action` | 60 |
| `dsherret/rust-toolchain-file` | 34 |
| `uraimo/run-on-arch-action` | 34 |
| `docker/login-action` | 31 |
| `denoland/setup-deno` | 29 |
| `actions/setup-go` | 28 |
| `open-security-tools/ost-simple-sts` | 26 |
| `actions/attest-build-provenance` | 22 |
| `github/gh-aw-actions/setup` | 22 |
| `actions/create-github-app-token` | 17 |
| `openai/codex-action` | 15 |
| `rui314/setup-mold` | 15 |
| `docker/metadata-action` | 14 |
| `actions/stale` | 11 |
| `CodSpeedHQ/action` | 10 |
| `docker/setup-buildx-action` | 10 |
| `cachix/install-nix-action` | 9 |
| `Mozilla-Actions/sccache-action` | 8 |
| `conda-incubator/setup-miniconda` | 8 |
| `jlumbroso/free-disk-space` | 8 |
| `prometheus/promci-setup` | 8 |
| `github/codeql-action/upload-sarif` | 7 |
| `google-github-actions/auth` | 7 |
| `google-github-actions/setup-gcloud` | 7 |
| `gr2m/create-or-update-pull-request-action` | 7 |
| `pnpm/action-setup` | 7 |

168 distinct actions (264 distinct pinned refs).

## Unsupported features, by in-scope workflows affected

| Feature | Workflows |
|---|---|
| `timeout.expression` | 8 |
| `runs_on.expression` | 3 |

## Failures that are not specific rejections

None. Every workflow either lowered or was rejected with a specific code.

## Every workflow

| Repository | Workflow | Result | Nodes | Unsupported |
|---|---|---|---|---|
| BurntSushi/ripgrep | `ci.yml` | out of scope | — | — |
| BurntSushi/ripgrep | `release.yml` | out of scope | — | — |
| actions/checkout | `check-dist.yml` | warnings | 12 | — |
| actions/checkout | `codeql-analysis.yml` | warnings | 12 | — |
| actions/checkout | `licensed.yml` | clean | 5 | — |
| actions/checkout | `publish-immutable-actions.yml` | warnings | 6 | — |
| actions/checkout | `test.yml` | out of scope | — | — |
| actions/checkout | `update-main-version.yml` | clean | 6 | — |
| actions/checkout | `update-test-ubuntu-git.yml` | warnings | 11 | — |
| astral-sh/ruff | `build-binaries.yml` | out of scope | — | — |
| astral-sh/ruff | `build-docker.yml` | warnings | 79 | — |
| astral-sh/ruff | `build-wasm.yml` | warnings | 16 | — |
| astral-sh/ruff | `ci.yaml` | out of scope | — | — |
| astral-sh/ruff | `daily_fuzz.yaml` | warnings | 20 | — |
| astral-sh/ruff | `memory_report.yaml` | warnings | 21 | — |
| astral-sh/ruff | `notify-dependents.yml` | warnings | 5 | — |
| astral-sh/ruff | `publish-crates.yml` | warnings | 8 | — |
| astral-sh/ruff | `publish-docs.yml` | warnings | 18 | — |
| astral-sh/ruff | `publish-mirror.yml` | warnings | 7 | — |
| astral-sh/ruff | `publish-playground.yml` | warnings | 28 | — |
| astral-sh/ruff | `publish-pypi.yml` | warnings | 9 | — |
| astral-sh/ruff | `publish-ty-playground.yml` | warnings | 29 | — |
| astral-sh/ruff | `publish-versions.yml` | warnings | 12 | — |
| astral-sh/ruff | `publish-wasm.yml` | warnings | 10 | — |
| astral-sh/ruff | `release.yml` | out of scope | — | — |
| astral-sh/ruff | `sync_typeshed.yaml` | out of scope | — | — |
| astral-sh/ruff | `ty-ecosystem-analyzer.yaml` | warnings | 53 | — |
| astral-sh/ruff | `ty-ecosystem-report.yaml` | warnings | 17 | — |
| astral-sh/ruff | `typing_conformance.yaml` | warnings | 21 | — |
| astral-sh/uv | `bench.yml` | warnings | 45 | — |
| astral-sh/uv | `build-dev-binaries.yml` | out of scope | — | — |
| astral-sh/uv | `build-docker.yml` | warnings | 70 | — |
| astral-sh/uv | `build-release-binaries.yml` | out of scope | — | — |
| astral-sh/uv | `check-docs.yml` | warnings | 14 | — |
| astral-sh/uv | `check-fmt.yml` | warnings | 16 | — |
| astral-sh/uv | `check-generated-files.yml` | warnings | 15 | — |
| astral-sh/uv | `check-lint.yml` | out of scope | — | — |
| astral-sh/uv | `check-lock.yml` | warnings | 8 | — |
| astral-sh/uv | `check-publish.yml` | warnings | 7 | — |
| astral-sh/uv | `check-release.yml` | clean | 7 | — |
| astral-sh/uv | `check-zizmor.yml` | warnings | 6 | — |
| astral-sh/uv | `ci.yml` | out of scope | — | — |
| astral-sh/uv | `diagnose-workflow-failure.yml` | warnings | 22 | — |
| astral-sh/uv | `fix-bug.yml` | warnings | 51 | — |
| astral-sh/uv | `issue-triage.yml` | warnings | 143 | — |
| astral-sh/uv | `plan.yml` | warnings | 6 | — |
| astral-sh/uv | `promote-pull-request.yml` | warnings | 35 | — |
| astral-sh/uv | `publish-crates.yml` | warnings | 8 | — |
| astral-sh/uv | `publish-docs.yml` | warnings | 20 | — |
| astral-sh/uv | `publish-mirror.yml` | warnings | 7 | — |
| astral-sh/uv | `publish-pypi.yml` | warnings | 18 | — |
| astral-sh/uv | `publish-versions.yml` | warnings | 12 | — |
| astral-sh/uv | `pull-request-conflicts.yml` | warnings | 41 | — |
| astral-sh/uv | `pull-request-labels.yml` | warnings | 23 | — |
| astral-sh/uv | `pull-request-security-review.yml` | warnings | 33 | — |
| astral-sh/uv | `rebase-conflicted-pull-request.yml` | warnings | 31 | — |
| astral-sh/uv | `release-prepare.yml` | warnings | 24 | — |
| astral-sh/uv | `release.yml` | out of scope | — | — |
| astral-sh/uv | `reproduce-bug.yml` | warnings | 55 | — |
| astral-sh/uv | `sync-python-releases.yml` | warnings | 18 | — |
| astral-sh/uv | `sync-uv-dev.yml` | warnings | 6 | — |
| astral-sh/uv | `sync-uv-security.yml` | warnings | 7 | — |
| astral-sh/uv | `test-ecosystem.yml` | warnings | 13 | — |
| astral-sh/uv | `test-integration.yml` | out of scope | — | — |
| astral-sh/uv | `test-smoke.yml` | out of scope | — | — |
| astral-sh/uv | `test-system.yml` | out of scope | — | — |
| astral-sh/uv | `test-windows-trampolines.yml` | out of scope | — | — |
| astral-sh/uv | `test.yml` | out of scope | — | — |
| astral-sh/uv | `update-issue-context.yml` | warnings | 28 | — |
| astral-sh/uv | `update-pull-request-parent.yml` | warnings | 7 | — |
| cli/cli | `agentics-maintenance.yml` | warnings | 160 | — |
| cli/cli | `bump-go.yml` | warnings | 7 | — |
| cli/cli | `codeql.yml` | warnings | 16 | — |
| cli/cli | `copilot-setup-steps.yml` | warnings | 6 | — |
| cli/cli | `dependabot-triage.lock.yml` | warnings | 231 | — |
| cli/cli | `deployment.yml` | out of scope | — | — |
| cli/cli | `go.yml` | out of scope | — | — |
| cli/cli | `govulncheck.yml` | warnings | 10 | — |
| cli/cli | `issue-triage.lock.yml` | warnings | 226 | — |
| cli/cli | `lint.yml` | warnings | 18 | — |
| cli/cli | `triage-issues.yml` | warnings | 64 | — |
| cli/cli | `triage-pull-requests.yml` | warnings | 45 | — |
| cli/cli | `triage-scheduled-tasks.yml` | warnings | 22 | — |
| denoland/deno | `cargo_publish.generated.yml` | warnings | 14 | — |
| denoland/deno | `ci.generated.yml` | out of scope | — | — |
| denoland/deno | `create_prerelease_tag.generated.yml` | warnings | 10 | — |
| denoland/deno | `ecosystem_compat_test.generated.yml` | out of scope | — | — |
| denoland/deno | `node_compat_test.generated.yml` | out of scope | — | — |
| denoland/deno | `npm_publish.generated.yml` | out of scope | — | — |
| denoland/deno | `post_publish.generated.yml` | clean | 14 | — |
| denoland/deno | `pr.generated.yml` | clean | 6 | — |
| denoland/deno | `promote_to_release.generated.yml` | out of scope | — | — |
| denoland/deno | `start_release.generated.yml` | clean | 8 | — |
| denoland/deno | `version_bump.generated.yml` | clean | 14 | — |
| django/django | `benchmark.yml` | warnings | 13 | — |
| django/django | `check-migrations.yml` | warnings | 9 | — |
| django/django | `check_commit_messages.yml` | warnings | 13 | — |
| django/django | `check_pr_quality.yml` | warnings | 9 | — |
| django/django | `coverage_comment.yml` | warnings | 8 | — |
| django/django | `coverage_tests.yml` | warnings | 15 | — |
| django/django | `docs.yml` | warnings | 11 | — |
| django/django | `labels.yml` | warnings | 5 | — |
| django/django | `linters.yml` | warnings | 40 | — |
| django/django | `new_contributor_pr.yml` | warnings | 5 | — |
| django/django | `playwright.yml` | warnings | 23 | — |
| django/django | `postgis.yml` | warnings | 12 | — |
| django/django | `python_matrix.yml` | warnings | 14 | — |
| django/django | `schedule_tests.yml` | out of scope | — | — |
| django/django | `schedules.yml` | warnings | 5 | — |
| django/django | `screenshots.yml` | warnings | 20 | — |
| django/django | `tests.yml` | out of scope | — | — |
| facebook/react | `compiler_discord_notify.yml` | warnings | 17 | — |
| facebook/react | `compiler_playground.yml` | warnings | 21 | — |
| facebook/react | `compiler_prereleases.yml` | warnings | 12 | — |
| facebook/react | `compiler_prereleases_manual.yml` | warnings | 15 | — |
| facebook/react | `compiler_prereleases_nightly.yml` | warnings | 15 | — |
| facebook/react | `compiler_rust.yml` | warnings | 22 | — |
| facebook/react | `compiler_typescript.yml` | warnings | 38 | — |
| facebook/react | `devtools_discord_notify.yml` | warnings | 17 | — |
| facebook/react | `devtools_regression_tests.yml` | warnings | 85 | — |
| facebook/react | `runtime_build_and_test.yml` | warnings | 495 | — |
| facebook/react | `runtime_commit_artifacts.yml` | warnings | 78 | — |
| facebook/react | `runtime_discord_notify.yml` | warnings | 17 | — |
| facebook/react | `runtime_eslint_plugin_e2e.yml` | warnings | 17 | — |
| facebook/react | `runtime_fuzz_tests.yml` | warnings | 8 | — |
| facebook/react | `runtime_release_from_ci.yml` | warnings | 32 | — |
| facebook/react | `runtime_sizebot_comment.yml` | warnings | 17 | — |
| facebook/react | `shared_check_maintainer.yml` | warnings | 5 | — |
| facebook/react | `shared_cleanup_merged_branch_caches.yml` | warnings | 3 | — |
| facebook/react | `shared_cleanup_stale_branch_caches.yml` | warnings | 3 | — |
| facebook/react | `shared_close_direct_sync_branch_prs.yml` | warnings | 5 | — |
| facebook/react | `shared_label_core_team_prs.yml` | warnings | 17 | — |
| facebook/react | `shared_lint.yml` | warnings | 48 | — |
| facebook/react | `shared_stale.yml` | warnings | 5 | — |
| hashicorp/terraform | `backport.yml` | warnings | 3 | — |
| hashicorp/terraform | `build-terraform-cli.yml` | callee only | — | — |
| hashicorp/terraform | `build.yml` | out of scope | — | — |
| hashicorp/terraform | `changelog-validation.yml` | warnings | 8 | — |
| hashicorp/terraform | `checks.yml` | warnings | 54 | — |
| hashicorp/terraform | `enforce-changelog.yml` | warnings | 11 | — |
| hashicorp/terraform | `equivalence-test-diff.yml` | warnings | 14 | — |
| hashicorp/terraform | `equivalence-test-manual-update.yml` | warnings | 14 | — |
| hashicorp/terraform | `equivalence-test-update.yml` | warnings | 17 | — |
| hashicorp/terraform | `issue-comment-created.yml` | warnings | 5 | — |
| hashicorp/terraform | `lock.yml` | warnings | 5 | — |
| nodejs/node | `auto-start-ci.yml` | warnings | 11 | — |
| nodejs/node | `benchmark.yml` | out of scope | — | — |
| nodejs/node | `build-shared.yml` | callee only | — | — |
| nodejs/node | `build-tarball.yml` | warnings | 30 | — |
| nodejs/node | `close-stalled.yml` | warnings | 5 | — |
| nodejs/node | `codeql.yml` | warnings | 12 | — |
| nodejs/node | `comment-labeled.yml` | warnings | 9 | — |
| nodejs/node | `commit-lint.yml` | warnings | 9 | — |
| nodejs/node | `commit-queue.yml` | warnings | 15 | — |
| nodejs/node | `coverage-linux-without-intl.yml` | warnings | 21 | — |
| nodejs/node | `coverage-linux.yml` | warnings | 21 | — |
| nodejs/node | `coverage-windows.yml` | out of scope | — | — |
| nodejs/node | `create-release-proposal.yml` | warnings | 13 | — |
| nodejs/node | `daily-wpt-fyi.yml` | warnings | 39 | — |
| nodejs/node | `daily.yml` | warnings | 7 | — |
| nodejs/node | `doc.yml` | warnings | 11 | — |
| nodejs/node | `find-inactive-collaborators.yml` | warnings | 10 | — |
| nodejs/node | `find-inactive-tsc.yml` | warnings | 13 | — |
| nodejs/node | `label-flaky-test-issue.yml` | warnings | 4 | — |
| nodejs/node | `label-pr.yml` | warnings | 5 | — |
| nodejs/node | `license-builder.yml` | warnings | 7 | — |
| nodejs/node | `lint-release-proposal.yml` | warnings | 8 | — |
| nodejs/node | `linters.yml` | warnings | 77 | — |
| nodejs/node | `major-release.yml` | warnings | 6 | — |
| nodejs/node | `nix-changes-comment.yml` | warnings | 7 | — |
| nodejs/node | `nix-changes.yml` | out of scope | — | — |
| nodejs/node | `notify-on-push.yml` | warnings | 14 | — |
| nodejs/node | `notify-on-review-wanted.yml` | warnings | 6 | — |
| nodejs/node | `post-release.yml` | warnings | 4 | — |
| nodejs/node | `scorecard.yml` | warnings | 15 | — |
| nodejs/node | `stale.yml` | warnings | 5 | — |
| nodejs/node | `stress-test.yml` | out of scope | — | — |
| nodejs/node | `test-internet.yml` | warnings | 14 | — |
| nodejs/node | `test-linux-quic.yml` | warnings | 16 | — |
| nodejs/node | `test-linux.yml` | warnings | 17 | — |
| nodejs/node | `test-macos.yml` | out of scope | — | — |
| nodejs/node | `test-shared.yml` | unsupported | — | `runs_on.expression` |
| nodejs/node | `timezone-update.yml` | warnings | 14 | — |
| nodejs/node | `tools.yml` | warnings | 14 | — |
| nodejs/node | `update-openssl.yml` | warnings | 11 | — |
| nodejs/node | `update-v8.yml` | warnings | 15 | — |
| nodejs/node | `update-wpt.yml` | warnings | 13 | — |
| ohmyzsh/ohmyzsh | `dependencies.yml` | warnings | 13 | — |
| ohmyzsh/ohmyzsh | `installer.yml` | out of scope | — | — |
| ohmyzsh/ohmyzsh | `main.yml` | warnings | 8 | — |
| ohmyzsh/ohmyzsh | `project.yml` | warnings | 12 | — |
| ohmyzsh/ohmyzsh | `scorecard.yml` | warnings | 15 | — |
| pola-rs/polars | `benchmark-remote.yml` | warnings | 14 | — |
| pola-rs/polars | `benchmark.yml` | warnings | 29 | — |
| pola-rs/polars | `changes-dsl-labeler.yml` | warnings | 5 | — |
| pola-rs/polars | `clear-caches.yml` | clean | 4 | — |
| pola-rs/polars | `docs-global.yml` | out of scope | — | — |
| pola-rs/polars | `docs-python.yml` | warnings | 24 | — |
| pola-rs/polars | `docs-rust.yml` | warnings | 13 | — |
| pola-rs/polars | `issue-labeler.yml` | warnings | 5 | — |
| pola-rs/polars | `lint-global.yml` | warnings | 11 | — |
| pola-rs/polars | `lint-python.yml` | warnings | 19 | — |
| pola-rs/polars | `lint-rust.yml` | warnings | 38 | — |
| pola-rs/polars | `pr-labeler.yml` | warnings | 11 | — |
| pola-rs/polars | `release-drafter.yml` | warnings | 8 | — |
| pola-rs/polars | `release-python.yml` | out of scope | — | — |
| pola-rs/polars | `release-rust.yml` | clean | 3 | — |
| pola-rs/polars | `test-bytecode-parser.yml` | warnings | 8 | — |
| pola-rs/polars | `test-coverage.yml` | out of scope | — | — |
| pola-rs/polars | `test-pyodide.yml` | warnings | 11 | — |
| pola-rs/polars | `test-python.yml` | out of scope | — | — |
| pola-rs/polars | `test-rust.yml` | out of scope | — | — |
| prometheus/prometheus | `approve-workflows.yml` | warnings | 5 | — |
| prometheus/prometheus | `automerge-dependabot.yml` | warnings | 6 | — |
| prometheus/prometheus | `buf-lint.yml` | warnings | 12 | — |
| prometheus/prometheus | `buf.yml` | warnings | 15 | — |
| prometheus/prometheus | `check_release_notes.yml` | warnings | 4 | — |
| prometheus/prometheus | `ci.yml` | out of scope | — | — |
| prometheus/prometheus | `codeql-analysis.yml` | warnings | 12 | — |
| prometheus/prometheus | `container_description.yml` | warnings | 15 | — |
| prometheus/prometheus | `fuzzing.yml` | warnings | 14 | — |
| prometheus/prometheus | `govulncheck.yml` | warnings | 6 | — |
| prometheus/prometheus | `lock.yml` | warnings | 5 | — |
| prometheus/prometheus | `prombench.yml` | warnings | 18 | — |
| prometheus/prometheus | `repo_sync.yml` | warnings | 4 | — |
| prometheus/prometheus | `scorecards.yml` | warnings | 12 | — |
| prometheus/prometheus | `stale.yml` | warnings | 5 | — |
| python/cpython | `add-issue-header.yml` | warnings | 5 | — |
| python/cpython | `build.yml` | out of scope | — | — |
| python/cpython | `jit.yml` | out of scope | — | — |
| python/cpython | `lint.yml` | warnings | 6 | — |
| python/cpython | `mypy.yml` | warnings | 9 | — |
| python/cpython | `new-bugs-announce-notifier.yml` | warnings | 9 | — |
| python/cpython | `require-pr-label.yml` | warnings | 16 | — |
| python/cpython | `reusable-check-c-api-docs.yml` | warnings | 7 | — |
| python/cpython | `reusable-check-html-ids.yml` | warnings | 18 | — |
| python/cpython | `reusable-cifuzz.yml` | warnings | 14 | — |
| python/cpython | `reusable-context.yml` | warnings | 12 | — |
| python/cpython | `reusable-docs.yml` | warnings | 60 | — |
| python/cpython | `reusable-emscripten.yml` | warnings | 24 | — |
| python/cpython | `reusable-install.yml` | warnings | 12 | — |
| python/cpython | `reusable-macos.yml` | callee only | — | — |
| python/cpython | `reusable-san.yml` | warnings | 21 | — |
| python/cpython | `reusable-ubuntu.yml` | callee only | — | — |
| python/cpython | `reusable-wasi.yml` | warnings | 21 | — |
| python/cpython | `reusable-windows.yml` | out of scope | — | — |
| python/cpython | `stale.yml` | warnings | 5 | — |
| python/cpython | `tail-call.yml` | out of scope | — | — |
| python/cpython | `verify-ensurepip-wheels.yml` | warnings | 7 | — |
| python/cpython | `verify-expat.yml` | warnings | 4 | — |
| rails/rails | `check-markdown-api.yml` | warnings | 7 | — |
| rails/rails | `devcontainer-shellcheck.yml` | warnings | 4 | — |
| rails/rails | `devcontainer-smoke-test.yml` | warnings | 29 | — |
| rails/rails | `labeler.yml` | warnings | 5 | — |
| rails/rails | `more-info-needed.yml` | warnings | 5 | — |
| rails/rails | `rail_inspector.yml` | warnings | 8 | — |
| rails/rails | `rails-new-docker.yml` | warnings | 14 | — |
| rails/rails | `rails_releaser_tests.yml` | warnings | 8 | — |
| rails/rails | `release.yml` | warnings | 16 | — |
| rails/rails | `stale.yml` | warnings | 5 | — |
| rust-lang/cargo | `audit.yml` | warnings | 6 | — |
| rust-lang/cargo | `contrib.yml` | warnings | 13 | — |
| rust-lang/cargo | `main.yml` | out of scope | — | — |
| rust-lang/cargo | `release.yml` | warnings | 4 | — |
| serde-rs/serde | `ci.yml` | out of scope | — | — |
| sharkdp/bat | `CICD.yml` | out of scope | — | — |
| sharkdp/bat | `require-changelog-for-PRs.yml` | clean | 6 | — |
| tailwindlabs/tailwindcss | `ci.yml` | out of scope | — | — |
| tailwindlabs/tailwindcss | `integration-tests.yml` | out of scope | — | — |
| tailwindlabs/tailwindcss | `prepare-release.yml` | out of scope | — | — |
| tailwindlabs/tailwindcss | `release.yml` | out of scope | — | — |
| tokio-rs/tokio | `audit.yml` | warnings | 6 | — |
| tokio-rs/tokio | `ci.yml` | out of scope | — | — |
| tokio-rs/tokio | `labeler.yml` | warnings | 5 | — |
| tokio-rs/tokio | `loom.yml` | warnings | 60 | — |
| tokio-rs/tokio | `pr-audit.yml` | warnings | 6 | — |
| tokio-rs/tokio | `stress-test.yml` | warnings | 14 | — |
| tokio-rs/tokio | `uring-kernel-version-test.yml` | clean | 11 | — |
| vercel/next.js | `automated_code_review.yml` | broken upstream | — | — |
| vercel/next.js | `build_and_deploy.yml` | unsupported | — | `runs_on.expression` |
| vercel/next.js | `build_and_test.yml` | out of scope | — | — |
| vercel/next.js | `build_reusable.yml` | unsupported | — | `timeout.expression` |
| vercel/next.js | `code_freeze.yml` | warnings | 9 | — |
| vercel/next.js | `create_release_branch.yml` | clean | 19 | — |
| vercel/next.js | `integration_tests_reusable.yml` | unsupported | — | `timeout.expression` |
| vercel/next.js | `issue_lock.yml` | warnings | 5 | — |
| vercel/next.js | `issue_reopen.yml` | warnings | 8 | — |
| vercel/next.js | `issue_stale.yml` | clean | 14 | — |
| vercel/next.js | `issue_wrong_template.yml` | clean | 8 | — |
| vercel/next.js | `popular.yml` | clean | 10 | — |
| vercel/next.js | `pr_ci_comment.yml` | warnings | 7 | — |
| vercel/next.js | `pr_stack_optimizer.yml` | clean | 6 | — |
| vercel/next.js | `pull_request_auto_label.yml` | warnings | 5 | — |
| vercel/next.js | `pull_request_stats.yml` | unsupported | — | `timeout.expression` |
| vercel/next.js | `release-next-rspack.yml` | unsupported | — | `runs_on.expression` |
| vercel/next.js | `retry_deploy_test.yml` | warnings | 8 | — |
| vercel/next.js | `retry_test.yml` | warnings | 14 | — |
| vercel/next.js | `rspack-nextjs-build-integration-tests.yml` | unsupported | — | `timeout.expression` |
| vercel/next.js | `rspack-nextjs-dev-integration-tests.yml` | unsupported | — | `timeout.expression` |
| vercel/next.js | `rspack-update-tests-manifest.yml` | clean | 30 | — |
| vercel/next.js | `sync_backport_canary_release.yml` | warnings | 23 | — |
| vercel/next.js | `test-turbopack-rust-bench-test.yml` | clean | 22 | — |
| vercel/next.js | `test_e2e_deploy_release.yml` | unsupported | — | `timeout.expression` |
| vercel/next.js | `test_e2e_project_reset_cron.yml` | clean | 9 | — |
| vercel/next.js | `test_examples.yml` | clean | 11 | — |
| vercel/next.js | `triage.yml` | warnings | 5 | — |
| vercel/next.js | `trigger_release.yml` | clean | 22 | — |
| vercel/next.js | `turbopack-benchmark.yml` | warnings | 53 | — |
| vercel/next.js | `turbopack-nextjs-build-integration-tests.yml` | unsupported | — | `timeout.expression` |
| vercel/next.js | `turbopack-nextjs-dev-integration-tests.yml` | unsupported | — | `timeout.expression` |
| vercel/next.js | `turbopack-update-tests-manifest.yml` | clean | 30 | — |
| vercel/next.js | `update_fonts_data.yml` | clean | 15 | — |
| vercel/next.js | `update_react.yml` | warnings | 15 | — |
| vercel/next.js | `update_react_poller.yml` | warnings | 9 | — |
| vercel/next.js | `upload-tests-manifest.yml` | clean | 11 | — |
| vercel/next.js | `upload_preview_tarballs.yml` | warnings | 18 | — |
