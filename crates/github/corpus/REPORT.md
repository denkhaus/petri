# Compatibility corpus report

316 workflows from 21 repositories.

| Result | Count | Share |
|---|---|---|
| lowered clean | 0 | 0% |
| lowered with warnings | 5 | 2% |
| rejected with a specific `unsupported.*` code | 311 | 98% |
| **failed for any other reason** | 0 | 0% |
| **panicked** | 0 | 0% |

## Remote actions, by frequency

Which actions the runner meets most. `uses:` references across all workflows; a workflow using an action three times counts three.

| Action | Uses |
|---|---|
| `actions/checkout` | 660 |
| `actions/upload-artifact` | 251 |
| `actions/download-artifact` | 207 |
| `actions/setup-node` | 124 |
| `Swatinem/rust-cache` | 111 |
| `actions/github-script` | 111 |
| `actions/setup-python` | 107 |
| `actions/cache/restore` | 91 |
| `actions/cache` | 79 |
| `dtolnay/rust-toolchain` | 70 |
| `actions/cache/save` | 50 |
| `taiki-e/install-action` | 50 |
| `astral-sh/setup-uv` | 49 |
| `PyO3/maturin-action` | 35 |
| `dsherret/rust-toolchain-file` | 34 |
| `denoland/setup-deno` | 29 |
| `actions/setup-go` | 26 |
| `github/gh-aw-actions/setup` | 22 |
| `open-security-tools/ost-simple-sts` | 20 |
| `actions/create-github-app-token` | 17 |
| `rui314/setup-mold` | 15 |
| `docker/login-action` | 13 |
| `uraimo/run-on-arch-action` | 12 |
| `actions/attest-build-provenance` | 11 |
| `actions/stale` | 11 |
| `cargo-bins/cargo-binstall` | 10 |
| `openai/codex-action` | 10 |
| `vercel/setup-turborepo-remote-cache-action` | 9 |
| `CodSpeedHQ/action` | 8 |
| `Mozilla-Actions/sccache-action` | 8 |
| `jlumbroso/free-disk-space` | 8 |
| `prometheus/promci-setup` | 8 |
| `actions-rust-lang/setup-rust-toolchain` | 7 |
| `cachix/install-nix-action` | 7 |
| `gr2m/create-or-update-pull-request-action` | 7 |
| `pnpm/action-setup` | 7 |
| `softprops/action-gh-release` | 7 |
| `step-security/harden-runner` | 7 |
| `docker/metadata-action` | 6 |
| `github/codeql-action/upload-sarif` | 6 |

152 distinct actions (250 distinct pinned refs).

## Unsupported features, by workflows affected

| Feature | Workflows |
|---|---|
| `action.remote` | 296 |
| `concurrency` | 108 |
| `workflow_call` | 92 |
| `inputs` | 59 |
| `runs_on.expression` | 47 |
| `environment` | 46 |
| `workflow_dispatch.inputs` | 46 |
| `runs_on.unknown` | 23 |
| `runs_on.windows` | 14 |
| `shell.bash` | 8 |
| `shell.pwsh` | 7 |
| `services` | 5 |
| `action.local_missing` | 4 |
| `action.docker` | 3 |
| `shell.cmd` | 3 |
| `shell.powershell` | 3 |
| `shell.python` | 3 |
| `timeout.expression` | 3 |
| `container.expression` | 2 |
| `container.options` | 2 |
| `step.background` | 2 |
| `yaml.anchors` | 2 |
| `expression.hashFiles` | 1 |
| `shell.nu` | 1 |
| `shell.wsl-bash` | 1 |
| `yaml.multiline_flow` | 1 |

## Failures that are not specific rejections

None. Every workflow either lowered or was rejected with a specific code.

## Every workflow

| Repository | Workflow | Result | Nodes | Unsupported |
|---|---|---|---|---|
| BurntSushi/ripgrep | `ci.yml` | unsupported | — | `action.remote`, `runs_on.expression` |
| BurntSushi/ripgrep | `release.yml` | unsupported | — | `action.remote`, `runs_on.expression` |
| actions/checkout | `check-dist.yml` | unsupported | — | `action.remote` |
| actions/checkout | `codeql-analysis.yml` | unsupported | — | `action.remote` |
| actions/checkout | `licensed.yml` | unsupported | — | `action.remote` |
| actions/checkout | `publish-immutable-actions.yml` | unsupported | — | `action.remote` |
| actions/checkout | `test.yml` | unsupported | — | `action.docker`, `action.local_missing`, `action.remote`, `container.options`, `runs_on.expression`, `services`, `shell.cmd` |
| actions/checkout | `update-main-version.yml` | unsupported | — | `action.remote`, `workflow_dispatch.inputs` |
| actions/checkout | `update-test-ubuntu-git.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_dispatch.inputs` |
| astral-sh/ruff | `build-binaries.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression`, `runs_on.unknown`, `workflow_call` |
| astral-sh/ruff | `build-docker.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_call` |
| astral-sh/ruff | `build-wasm.yml` | unsupported | — | `action.remote`, `concurrency`, `workflow_call` |
| astral-sh/ruff | `ci.yaml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression`, `runs_on.unknown`, `timeout.expression` |
| astral-sh/ruff | `daily_fuzz.yaml` | unsupported | — | `action.remote`, `concurrency` |
| astral-sh/ruff | `memory_report.yaml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression` |
| astral-sh/ruff | `notify-dependents.yml` | unsupported | — | `action.remote`, `environment`, `workflow_call` |
| astral-sh/ruff | `publish-crates.yml` | unsupported | — | `action.remote`, `environment`, `workflow_call` |
| astral-sh/ruff | `publish-docs.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/ruff | `publish-mirror.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_call` |
| astral-sh/ruff | `publish-playground.yml` | unsupported | — | `action.remote`, `environment`, `workflow_call` |
| astral-sh/ruff | `publish-pypi.yml` | unsupported | — | `action.remote`, `environment`, `workflow_call` |
| astral-sh/ruff | `publish-ty-playground.yml` | unsupported | — | `action.remote`, `concurrency`, `environment` |
| astral-sh/ruff | `publish-versions.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_call` |
| astral-sh/ruff | `publish-wasm.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_call` |
| astral-sh/ruff | `release.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `runs_on.unknown`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/ruff | `sync_typeshed.yaml` | unsupported | — | `action.remote`, `runs_on.expression`, `runs_on.windows` |
| astral-sh/ruff | `ty-ecosystem-analyzer.yaml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression` |
| astral-sh/ruff | `ty-ecosystem-report.yaml` | unsupported | — | `action.remote`, `runs_on.expression` |
| astral-sh/ruff | `typing_conformance.yaml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression` |
| astral-sh/uv | `bench.yml` | unsupported | — | `action.remote`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `build-dev-binaries.yml` | unsupported | — | `action.remote`, `runs_on.unknown`, `runs_on.windows`, `workflow_call` |
| astral-sh/uv | `build-docker.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_call` |
| astral-sh/uv | `build-release-binaries.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression`, `runs_on.unknown`, `timeout.expression`, `workflow_call` |
| astral-sh/uv | `check-docs.yml` | unsupported | — | `action.remote`, `workflow_call` |
| astral-sh/uv | `check-fmt.yml` | unsupported | — | `action.remote`, `workflow_call` |
| astral-sh/uv | `check-generated-files.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_call` |
| astral-sh/uv | `check-lint.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `check-lock.yml` | unsupported | — | `action.remote`, `workflow_call` |
| astral-sh/uv | `check-publish.yml` | unsupported | — | `action.remote`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `check-release.yml` | unsupported | — | `action.remote`, `workflow_call` |
| astral-sh/uv | `check-zizmor.yml` | unsupported | — | `action.remote`, `workflow_call` |
| astral-sh/uv | `ci.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `shell.bash`, `workflow_call` |
| astral-sh/uv | `diagnose-workflow-failure.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `inputs`, `workflow_dispatch.inputs` |
| astral-sh/uv | `fix-bug.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `inputs`, `runs_on.expression`, `workflow_call` |
| astral-sh/uv | `issue-triage.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `plan.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `promote-pull-request.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `publish-crates.yml` | unsupported | — | `action.remote`, `environment`, `workflow_call` |
| astral-sh/uv | `publish-docs.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `publish-mirror.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_call` |
| astral-sh/uv | `publish-pypi.yml` | unsupported | — | `action.remote`, `environment`, `workflow_call` |
| astral-sh/uv | `publish-versions.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_call` |
| astral-sh/uv | `pull-request-conflicts.yml` | unsupported | — | `action.remote`, `concurrency`, `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `pull-request-labels.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `inputs`, `workflow_dispatch.inputs` |
| astral-sh/uv | `pull-request-security-review.yml` | unsupported | — | `action.remote`, `environment`, `workflow_call` |
| astral-sh/uv | `rebase-conflicted-pull-request.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `release-prepare.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_dispatch.inputs` |
| astral-sh/uv | `release.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `runs_on.unknown`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `reproduce-bug.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `inputs`, `runs_on.expression`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `sync-python-releases.yml` | unsupported | — | `action.remote`, `environment` |
| astral-sh/uv | `sync-uv-dev.yml` | unsupported | — | `action.remote`, `concurrency`, `environment` |
| astral-sh/uv | `sync-uv-security.yml` | unsupported | — | `action.remote`, `concurrency`, `environment` |
| astral-sh/uv | `test-ecosystem.yml` | unsupported | — | `action.remote`, `workflow_call` |
| astral-sh/uv | `test-integration.yml` | unsupported | — | `action.remote`, `environment`, `runs_on.expression`, `runs_on.unknown`, `runs_on.windows`, `shell.bash`, `shell.nu`, `shell.pwsh`, `shell.python`, `shell.wsl-bash`, `workflow_call` |
| astral-sh/uv | `test-smoke.yml` | unsupported | — | `action.remote`, `runs_on.unknown`, `runs_on.windows`, `workflow_call` |
| astral-sh/uv | `test-system.yml` | unsupported | — | `action.remote`, `container.expression`, `container.options`, `runs_on.expression`, `runs_on.windows`, `shell.bash`, `shell.pwsh`, `workflow_call` |
| astral-sh/uv | `test-windows-trampolines.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.expression`, `runs_on.windows`, `workflow_call` |
| astral-sh/uv | `test.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `update-issue-context.yml` | unsupported | — | `action.remote`, `concurrency`, `environment` |
| astral-sh/uv | `update-pull-request-parent.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_call` |
| cli/cli | `agentics-maintenance.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| cli/cli | `bump-go.yml` | unsupported | — | `action.remote` |
| cli/cli | `codeql.yml` | unsupported | — | `action.remote` |
| cli/cli | `copilot-setup-steps.yml` | unsupported | — | `action.remote` |
| cli/cli | `dependabot-triage.lock.yml` | unsupported | — | `action.remote`, `concurrency`, `workflow_dispatch.inputs` |
| cli/cli | `deployment.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `inputs`, `runs_on.windows`, `shell.pwsh`, `workflow_dispatch.inputs` |
| cli/cli | `go.yml` | unsupported | — | `action.remote`, `runs_on.expression` |
| cli/cli | `govulncheck.yml` | unsupported | — | `action.remote` |
| cli/cli | `issue-triage.lock.yml` | unsupported | — | `action.remote`, `concurrency`, `workflow_dispatch.inputs` |
| cli/cli | `lint.yml` | unsupported | — | `action.remote` |
| cli/cli | `triage-issues.yml` | unsupported | — | `workflow_call` |
| cli/cli | `triage-pull-requests.yml` | unsupported | — | `workflow_call` |
| cli/cli | `triage-scheduled-tasks.yml` | unsupported | — | `workflow_call` |
| denoland/deno | `cargo_publish.generated.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.unknown` |
| denoland/deno | `ci.generated.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `runs_on.expression`, `runs_on.windows`, `shell.pwsh` |
| denoland/deno | `create_prerelease_tag.generated.yml` | unsupported | — | `action.remote`, `concurrency` |
| denoland/deno | `ecosystem_compat_test.generated.yml` | unsupported | — | `action.remote`, `runs_on.expression` |
| denoland/deno | `node_compat_test.generated.yml` | unsupported | — | `action.remote`, `runs_on.expression` |
| denoland/deno | `npm_publish.generated.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.expression`, `shell.cmd`, `shell.pwsh`, `workflow_dispatch.inputs` |
| denoland/deno | `post_publish.generated.yml` | unsupported | — | `action.remote` |
| denoland/deno | `pr.generated.yml` | unsupported | — | `action.remote` |
| denoland/deno | `promote_to_release.generated.yml` | unsupported | — | `action.remote`, `environment`, `runs_on.windows`, `shell.pwsh`, `workflow_dispatch.inputs` |
| denoland/deno | `start_release.generated.yml` | unsupported | — | `action.remote`, `workflow_dispatch.inputs` |
| denoland/deno | `version_bump.generated.yml` | unsupported | — | `action.remote`, `workflow_dispatch.inputs` |
| django/django | `benchmark.yml` | unsupported | — | `action.remote`, `shell.bash` |
| django/django | `check-migrations.yml` | unsupported | — | `action.remote`, `concurrency`, `services` |
| django/django | `check_commit_messages.yml` | unsupported | — | `action.remote`, `concurrency` |
| django/django | `check_pr_quality.yml` | unsupported | — | `action.remote`, `concurrency` |
| django/django | `coverage_comment.yml` | unsupported | — | `action.remote` |
| django/django | `coverage_tests.yml` | unsupported | — | `action.remote`, `concurrency`, `services` |
| django/django | `docs.yml` | unsupported | — | `action.remote`, `concurrency` |
| django/django | `labels.yml` | unsupported | — | `action.remote`, `concurrency` |
| django/django | `linters.yml` | unsupported | — | `action.remote`, `concurrency` |
| django/django | `new_contributor_pr.yml` | unsupported | — | `action.remote` |
| django/django | `playwright.yml` | unsupported | — | `action.remote`, `concurrency`, `services` |
| django/django | `postgis.yml` | unsupported | — | `action.remote`, `concurrency`, `shell.bash` |
| django/django | `python_matrix.yml` | unsupported | — | `action.remote`, `concurrency` |
| django/django | `schedule_tests.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.windows`, `services` |
| django/django | `schedules.yml` | unsupported | — | `action.remote`, `environment` |
| django/django | `screenshots.yml` | unsupported | — | `action.remote`, `concurrency` |
| django/django | `tests.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.windows` |
| facebook/react | `compiler_discord_notify.yml` | unsupported | — | `action.remote`, `workflow_call` |
| facebook/react | `compiler_playground.yml` | unsupported | — | `action.remote`, `concurrency` |
| facebook/react | `compiler_prereleases.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_call` |
| facebook/react | `compiler_prereleases_manual.yml` | unsupported | — | `workflow_call`, `workflow_dispatch.inputs` |
| facebook/react | `compiler_prereleases_nightly.yml` | unsupported | — | `workflow_call` |
| facebook/react | `compiler_rust.yml` | unsupported | — | `action.remote`, `concurrency` |
| facebook/react | `compiler_typescript.yml` | unsupported | — | `action.remote`, `concurrency` |
| facebook/react | `devtools_discord_notify.yml` | unsupported | — | `action.remote`, `workflow_call` |
| facebook/react | `devtools_regression_tests.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_dispatch.inputs` |
| facebook/react | `runtime_build_and_test.yml` | unsupported | — | `action.remote`, `concurrency`, `step.background` |
| facebook/react | `runtime_commit_artifacts.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_dispatch.inputs` |
| facebook/react | `runtime_discord_notify.yml` | unsupported | — | `action.remote`, `workflow_call` |
| facebook/react | `runtime_eslint_plugin_e2e.yml` | unsupported | — | `action.remote`, `concurrency` |
| facebook/react | `runtime_fuzz_tests.yml` | unsupported | — | `action.remote` |
| facebook/react | `runtime_release_from_ci.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_dispatch.inputs` |
| facebook/react | `runtime_sizebot_comment.yml` | unsupported | — | `action.remote`, `concurrency`, `expression.hashFiles` |
| facebook/react | `shared_check_maintainer.yml` | unsupported | — | `action.remote`, `workflow_call` |
| facebook/react | `shared_cleanup_merged_branch_caches.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| facebook/react | `shared_cleanup_stale_branch_caches.yml` | warnings | 3 | — |
| facebook/react | `shared_close_direct_sync_branch_prs.yml` | unsupported | — | `action.remote` |
| facebook/react | `shared_label_core_team_prs.yml` | unsupported | — | `action.remote`, `workflow_call` |
| facebook/react | `shared_lint.yml` | unsupported | — | `action.remote`, `concurrency` |
| facebook/react | `shared_stale.yml` | unsupported | — | `action.remote` |
| hashicorp/terraform | `backport.yml` | warnings | 3 | — |
| hashicorp/terraform | `build-terraform-cli.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.expression`, `workflow_call` |
| hashicorp/terraform | `build.yml` | unsupported | — | `action.remote`, `runs_on.expression`, `shell.cmd`, `workflow_call` |
| hashicorp/terraform | `changelog-validation.yml` | unsupported | — | `action.remote` |
| hashicorp/terraform | `checks.yml` | unsupported | — | `action.remote` |
| hashicorp/terraform | `enforce-changelog.yml` | unsupported | — | `action.remote`, `concurrency` |
| hashicorp/terraform | `equivalence-test-diff.yml` | unsupported | — | `action.remote`, `shell.bash` |
| hashicorp/terraform | `equivalence-test-manual-update.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_dispatch.inputs` |
| hashicorp/terraform | `equivalence-test-update.yml` | unsupported | — | `action.remote` |
| hashicorp/terraform | `issue-comment-created.yml` | unsupported | — | `action.remote` |
| hashicorp/terraform | `lock.yml` | unsupported | — | `action.remote` |
| nodejs/node | `auto-start-ci.yml` | unsupported | — | `action.remote`, `concurrency` |
| nodejs/node | `benchmark.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.expression`, `workflow_dispatch.inputs` |
| nodejs/node | `build-shared.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.expression`, `workflow_call` |
| nodejs/node | `build-tarball.yml` | unsupported | — | `action.remote`, `concurrency` |
| nodejs/node | `close-stalled.yml` | unsupported | — | `action.remote` |
| nodejs/node | `codeql.yml` | unsupported | — | `action.remote` |
| nodejs/node | `comment-labeled.yml` | warnings | 9 | — |
| nodejs/node | `commit-lint.yml` | unsupported | — | `action.remote` |
| nodejs/node | `commit-queue.yml` | unsupported | — | `action.remote`, `concurrency` |
| nodejs/node | `coverage-linux-without-intl.yml` | unsupported | — | `action.remote`, `concurrency` |
| nodejs/node | `coverage-linux.yml` | unsupported | — | `action.remote`, `concurrency` |
| nodejs/node | `coverage-windows.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.windows` |
| nodejs/node | `create-release-proposal.yml` | unsupported | — | `action.remote`, `concurrency`, `inputs`, `workflow_dispatch.inputs` |
| nodejs/node | `daily-wpt-fyi.yml` | unsupported | — | `action.remote` |
| nodejs/node | `daily.yml` | unsupported | — | `action.remote` |
| nodejs/node | `doc.yml` | unsupported | — | `action.remote`, `concurrency` |
| nodejs/node | `find-inactive-collaborators.yml` | unsupported | — | `action.remote` |
| nodejs/node | `find-inactive-tsc.yml` | unsupported | — | `action.remote` |
| nodejs/node | `label-flaky-test-issue.yml` | warnings | 4 | — |
| nodejs/node | `label-pr.yml` | unsupported | — | `action.remote` |
| nodejs/node | `license-builder.yml` | unsupported | — | `action.remote` |
| nodejs/node | `lint-release-proposal.yml` | unsupported | — | `action.remote`, `concurrency` |
| nodejs/node | `linters.yml` | unsupported | — | `action.remote`, `concurrency` |
| nodejs/node | `major-release.yml` | warnings | 6 | — |
| nodejs/node | `nix-changes-comment.yml` | unsupported | — | `action.remote` |
| nodejs/node | `nix-changes.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression` |
| nodejs/node | `notify-on-push.yml` | unsupported | — | `action.remote` |
| nodejs/node | `notify-on-review-wanted.yml` | unsupported | — | `action.remote` |
| nodejs/node | `post-release.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| nodejs/node | `scorecard.yml` | unsupported | — | `action.remote` |
| nodejs/node | `stale.yml` | unsupported | — | `action.remote` |
| nodejs/node | `stress-test.yml` | unsupported | — | `action.local_missing`, `action.remote`, `inputs`, `runs_on.expression`, `workflow_dispatch.inputs` |
| nodejs/node | `test-internet.yml` | unsupported | — | `action.remote`, `concurrency` |
| nodejs/node | `test-linux-quic.yml` | unsupported | — | `action.local_missing`, `action.remote`, `concurrency` |
| nodejs/node | `test-linux.yml` | unsupported | — | `action.local_missing`, `action.remote`, `concurrency`, `runs_on.expression` |
| nodejs/node | `test-macos.yml` | unsupported | — | `action.remote`, `concurrency` |
| nodejs/node | `test-shared.yml` | unsupported | — | `action.remote`, `concurrency`, `workflow_call` |
| nodejs/node | `timezone-update.yml` | unsupported | — | `action.remote` |
| nodejs/node | `tools.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_dispatch.inputs` |
| nodejs/node | `update-openssl.yml` | unsupported | — | `action.remote` |
| nodejs/node | `update-v8.yml` | unsupported | — | `action.remote` |
| nodejs/node | `update-wpt.yml` | unsupported | — | `action.remote`, `workflow_dispatch.inputs` |
| ohmyzsh/ohmyzsh | `dependencies.yml` | unsupported | — | `action.remote` |
| ohmyzsh/ohmyzsh | `installer.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `runs_on.expression` |
| ohmyzsh/ohmyzsh | `main.yml` | unsupported | — | `action.remote`, `concurrency` |
| ohmyzsh/ohmyzsh | `project.yml` | unsupported | — | `action.remote`, `concurrency` |
| ohmyzsh/ohmyzsh | `scorecard.yml` | unsupported | — | `action.remote` |
| pola-rs/polars | `benchmark-remote.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.unknown` |
| pola-rs/polars | `benchmark.yml` | unsupported | — | `action.remote`, `concurrency` |
| pola-rs/polars | `changes-dsl-labeler.yml` | unsupported | — | `action.remote` |
| pola-rs/polars | `clear-caches.yml` | unsupported | — | `action.remote` |
| pola-rs/polars | `docs-global.yml` | unsupported | — | `action.remote` |
| pola-rs/polars | `docs-python.yml` | unsupported | — | `action.remote`, `concurrency`, `workflow_dispatch.inputs` |
| pola-rs/polars | `docs-rust.yml` | unsupported | — | `action.remote`, `concurrency` |
| pola-rs/polars | `issue-labeler.yml` | unsupported | — | `action.remote` |
| pola-rs/polars | `lint-global.yml` | unsupported | — | `action.remote`, `concurrency` |
| pola-rs/polars | `lint-python.yml` | unsupported | — | `action.remote`, `concurrency` |
| pola-rs/polars | `lint-rust.yml` | unsupported | — | `action.remote`, `concurrency` |
| pola-rs/polars | `pr-labeler.yml` | unsupported | — | `action.remote` |
| pola-rs/polars | `release-drafter.yml` | unsupported | — | `action.remote`, `workflow_dispatch.inputs` |
| pola-rs/polars | `release-python.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `inputs`, `runs_on.expression`, `shell.powershell`, `workflow_dispatch.inputs` |
| pola-rs/polars | `release-rust.yml` | unsupported | — | `action.remote` |
| pola-rs/polars | `test-bytecode-parser.yml` | unsupported | — | `action.remote`, `concurrency` |
| pola-rs/polars | `test-coverage.yml` | unsupported | — | `action.remote`, `concurrency` |
| pola-rs/polars | `test-pyodide.yml` | unsupported | — | `action.remote`, `concurrency` |
| pola-rs/polars | `test-python.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression`, `shell.powershell` |
| pola-rs/polars | `test-rust.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression` |
| prometheus/prometheus | `approve-workflows.yml` | unsupported | — | `action.remote` |
| prometheus/prometheus | `automerge-dependabot.yml` | unsupported | — | `action.remote`, `concurrency` |
| prometheus/prometheus | `buf-lint.yml` | unsupported | — | `action.remote` |
| prometheus/prometheus | `buf.yml` | unsupported | — | `action.remote` |
| prometheus/prometheus | `check_release_notes.yml` | unsupported | — | `action.remote` |
| prometheus/prometheus | `ci.yml` | unsupported | — | `action.remote`, `container.expression`, `runs_on.windows`, `shell.powershell`, `workflow_call` |
| prometheus/prometheus | `codeql-analysis.yml` | unsupported | — | `action.remote`, `workflow_call` |
| prometheus/prometheus | `container_description.yml` | unsupported | — | `action.remote` |
| prometheus/prometheus | `fuzzing.yml` | unsupported | — | `action.remote`, `workflow_call` |
| prometheus/prometheus | `govulncheck.yml` | unsupported | — | `action.remote` |
| prometheus/prometheus | `lock.yml` | unsupported | — | `action.remote`, `concurrency` |
| prometheus/prometheus | `prombench.yml` | unsupported | — | `action.docker` |
| prometheus/prometheus | `repo_sync.yml` | unsupported | — | `action.remote` |
| prometheus/prometheus | `scorecards.yml` | unsupported | — | `action.remote` |
| prometheus/prometheus | `stale.yml` | unsupported | — | `action.remote` |
| python/cpython | `add-issue-header.yml` | unsupported | — | `action.remote` |
| python/cpython | `build.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression`, `runs_on.unknown`, `workflow_call` |
| python/cpython | `jit.yml` | unsupported | — | `yaml.anchors` |
| python/cpython | `lint.yml` | unsupported | — | `action.remote`, `concurrency` |
| python/cpython | `mypy.yml` | unsupported | — | `yaml.multiline_flow` |
| python/cpython | `new-bugs-announce-notifier.yml` | unsupported | — | `action.remote` |
| python/cpython | `require-pr-label.yml` | unsupported | — | `action.remote` |
| python/cpython | `reusable-check-c-api-docs.yml` | unsupported | — | `action.remote`, `workflow_call` |
| python/cpython | `reusable-check-html-ids.yml` | unsupported | — | `action.remote`, `workflow_call` |
| python/cpython | `reusable-cifuzz.yml` | unsupported | — | `action.remote`, `workflow_call` |
| python/cpython | `reusable-context.yml` | unsupported | — | `action.remote`, `workflow_call` |
| python/cpython | `reusable-docs.yml` | unsupported | — | `action.remote`, `concurrency`, `workflow_call` |
| python/cpython | `reusable-emscripten.yml` | unsupported | — | `action.remote`, `shell.python`, `workflow_call` |
| python/cpython | `reusable-install.yml` | unsupported | — | `action.remote`, `runs_on.unknown`, `workflow_call` |
| python/cpython | `reusable-macos.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.expression`, `workflow_call` |
| python/cpython | `reusable-san.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_call` |
| python/cpython | `reusable-ubuntu.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.expression`, `workflow_call` |
| python/cpython | `reusable-wasi.yml` | unsupported | — | `action.remote`, `runs_on.unknown`, `shell.python`, `workflow_call` |
| python/cpython | `reusable-windows.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.expression`, `workflow_call` |
| python/cpython | `stale.yml` | unsupported | — | `action.remote` |
| python/cpython | `tail-call.yml` | unsupported | — | `yaml.anchors` |
| python/cpython | `verify-ensurepip-wheels.yml` | unsupported | — | `action.remote`, `concurrency` |
| python/cpython | `verify-expat.yml` | unsupported | — | `action.remote`, `concurrency` |
| rails/rails | `check-markdown-api.yml` | unsupported | — | `action.remote` |
| rails/rails | `devcontainer-shellcheck.yml` | unsupported | — | `action.remote` |
| rails/rails | `devcontainer-smoke-test.yml` | unsupported | — | `action.remote` |
| rails/rails | `labeler.yml` | unsupported | — | `action.remote` |
| rails/rails | `more-info-needed.yml` | unsupported | — | `action.remote` |
| rails/rails | `rail_inspector.yml` | unsupported | — | `action.remote` |
| rails/rails | `rails-new-docker.yml` | unsupported | — | `action.remote` |
| rails/rails | `rails_releaser_tests.yml` | unsupported | — | `action.remote` |
| rails/rails | `release.yml` | unsupported | — | `action.remote`, `environment` |
| rails/rails | `stale.yml` | unsupported | — | `action.remote` |
| rust-lang/cargo | `audit.yml` | unsupported | — | `action.remote` |
| rust-lang/cargo | `contrib.yml` | unsupported | — | `action.remote`, `concurrency`, `environment` |
| rust-lang/cargo | `main.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression`, `shell.pwsh` |
| rust-lang/cargo | `release.yml` | unsupported | — | `action.remote`, `concurrency`, `environment` |
| serde-rs/serde | `ci.yml` | unsupported | — | `action.remote`, `runs_on.expression`, `runs_on.windows` |
| sharkdp/bat | `CICD.yml` | unsupported | — | `action.remote`, `runs_on.expression` |
| sharkdp/bat | `require-changelog-for-PRs.yml` | unsupported | — | `action.remote` |
| tailwindlabs/tailwindcss | `ci.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression` |
| tailwindlabs/tailwindcss | `integration-tests.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression` |
| tailwindlabs/tailwindcss | `prepare-release.yml` | unsupported | — | `action.remote`, `concurrency`, `inputs`, `runs_on.expression`, `workflow_dispatch.inputs` |
| tailwindlabs/tailwindcss | `release.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression`, `workflow_dispatch.inputs` |
| tokio-rs/tokio | `audit.yml` | unsupported | — | `action.remote` |
| tokio-rs/tokio | `ci.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.expression`, `workflow_call` |
| tokio-rs/tokio | `labeler.yml` | unsupported | — | `action.remote`, `concurrency` |
| tokio-rs/tokio | `loom.yml` | unsupported | — | `action.remote`, `concurrency` |
| tokio-rs/tokio | `pr-audit.yml` | unsupported | — | `action.remote`, `concurrency` |
| tokio-rs/tokio | `stress-test.yml` | unsupported | — | `action.remote`, `concurrency` |
| tokio-rs/tokio | `uring-kernel-version-test.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_call` |
| vercel/next.js | `automated_code_review.yml` | unsupported | — | `action.remote`, `concurrency` |
| vercel/next.js | `build_and_deploy.yml` | unsupported | — | `action.remote`, `concurrency`, `environment`, `runs_on.expression`, `runs_on.unknown`, `shell.bash` |
| vercel/next.js | `build_and_test.yml` | unsupported | — | `action.remote`, `concurrency`, `workflow_call` |
| vercel/next.js | `build_reusable.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.expression`, `shell.bash`, `step.background`, `timeout.expression`, `workflow_call` |
| vercel/next.js | `code_freeze.yml` | unsupported | — | `action.remote`, `environment`, `workflow_dispatch.inputs` |
| vercel/next.js | `create_release_branch.yml` | unsupported | — | `action.remote`, `workflow_dispatch.inputs` |
| vercel/next.js | `integration_tests_reusable.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.unknown`, `workflow_call` |
| vercel/next.js | `issue_lock.yml` | unsupported | — | `action.remote`, `concurrency` |
| vercel/next.js | `issue_reopen.yml` | unsupported | — | `action.remote`, `concurrency` |
| vercel/next.js | `issue_stale.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `issue_wrong_template.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `popular.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `pr_ci_comment.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `pr_stack_optimizer.yml` | unsupported | — | `action.remote`, `workflow_call` |
| vercel/next.js | `pull_request_auto_label.yml` | unsupported | — | `action.remote`, `concurrency` |
| vercel/next.js | `pull_request_stats.yml` | unsupported | — | `action.docker`, `action.remote`, `concurrency`, `runs_on.unknown`, `workflow_call` |
| vercel/next.js | `release-next-rspack.yml` | unsupported | — | `action.remote`, `environment`, `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| vercel/next.js | `retry_deploy_test.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `retry_test.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `rspack-nextjs-build-integration-tests.yml` | unsupported | — | `workflow_call` |
| vercel/next.js | `rspack-nextjs-dev-integration-tests.yml` | unsupported | — | `workflow_call` |
| vercel/next.js | `rspack-update-tests-manifest.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `sync_backport_canary_release.yml` | unsupported | — | `action.remote`, `concurrency`, `inputs`, `workflow_dispatch.inputs` |
| vercel/next.js | `test-turbopack-rust-bench-test.yml` | unsupported | — | `action.remote`, `inputs`, `runs_on.expression`, `workflow_call` |
| vercel/next.js | `test_e2e_deploy_release.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| vercel/next.js | `test_e2e_project_reset_cron.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `test_examples.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_dispatch.inputs` |
| vercel/next.js | `triage.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `trigger_release.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_dispatch.inputs` |
| vercel/next.js | `turbopack-benchmark.yml` | unsupported | — | `action.remote`, `concurrency`, `runs_on.unknown` |
| vercel/next.js | `turbopack-nextjs-build-integration-tests.yml` | unsupported | — | `workflow_call` |
| vercel/next.js | `turbopack-nextjs-dev-integration-tests.yml` | unsupported | — | `workflow_call` |
| vercel/next.js | `turbopack-update-tests-manifest.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `update_fonts_data.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `update_react.yml` | unsupported | — | `action.remote`, `inputs`, `workflow_dispatch.inputs` |
| vercel/next.js | `update_react_poller.yml` | unsupported | — | `action.remote`, `concurrency` |
| vercel/next.js | `upload-tests-manifest.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `upload_preview_tarballs.yml` | unsupported | — | `action.remote` |
