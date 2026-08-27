# Compatibility corpus report

316 workflows from 21 repositories.

Remote `uses:` references resolve through the action snapshot: 276 references, 1 of them unavailable. Refresh with `cargo test -p petri-github-acceptance --test snapshot -- --ignored`.

| Result | Count | Share |
|---|---|---|
| lowered clean | 14 | 4% |
| lowered with warnings | 121 | 38% |
| rejected with a specific `unsupported.*` code | 181 | 57% |
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
| `workflow_call` | 92 |
| `inputs` | 70 |
| `runs_on.expression` | 47 |
| `workflow_dispatch.inputs` | 46 |
| `runs_on.unknown` | 22 |
| `action.docker` | 18 |
| `runs_on.macos` | 14 |
| `runs_on.windows` | 14 |
| `shell.powershell` | 11 |
| `services` | 5 |
| `action.local_missing` | 4 |
| `shell.cmd` | 3 |
| `timeout.expression` | 3 |
| `action.nested_local` | 2 |
| `container.expression` | 2 |
| `container.options` | 2 |
| `step.background` | 2 |
| `yaml.anchors` | 2 |
| `action.remote` | 1 |
| `yaml.multiline_flow` | 1 |

## Failures that are not specific rejections

None. Every workflow either lowered or was rejected with a specific code.

## Every workflow

| Repository | Workflow | Result | Nodes | Unsupported |
|---|---|---|---|---|
| BurntSushi/ripgrep | `ci.yml` | unsupported | — | `runs_on.expression` |
| BurntSushi/ripgrep | `release.yml` | unsupported | — | `runs_on.expression` |
| actions/checkout | `check-dist.yml` | warnings | 10 | — |
| actions/checkout | `codeql-analysis.yml` | warnings | 11 | — |
| actions/checkout | `licensed.yml` | clean | 6 | — |
| actions/checkout | `publish-immutable-actions.yml` | warnings | 5 | — |
| actions/checkout | `test.yml` | unsupported | — | `action.docker`, `action.local_missing`, `container.options`, `runs_on.expression`, `services`, `shell.cmd` |
| actions/checkout | `update-main-version.yml` | unsupported | — | `workflow_dispatch.inputs` |
| actions/checkout | `update-test-ubuntu-git.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| astral-sh/ruff | `build-binaries.yml` | unsupported | — | `runs_on.expression`, `runs_on.unknown`, `workflow_call` |
| astral-sh/ruff | `build-docker.yml` | unsupported | — | `inputs`, `workflow_call` |
| astral-sh/ruff | `build-wasm.yml` | unsupported | — | `workflow_call` |
| astral-sh/ruff | `ci.yaml` | unsupported | — | `runs_on.expression`, `runs_on.unknown`, `shell.powershell`, `timeout.expression` |
| astral-sh/ruff | `daily_fuzz.yaml` | warnings | 15 | — |
| astral-sh/ruff | `memory_report.yaml` | unsupported | — | `runs_on.expression` |
| astral-sh/ruff | `notify-dependents.yml` | unsupported | — | `workflow_call` |
| astral-sh/ruff | `publish-crates.yml` | unsupported | — | `workflow_call` |
| astral-sh/ruff | `publish-docs.yml` | unsupported | — | `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/ruff | `publish-mirror.yml` | unsupported | — | `inputs`, `workflow_call` |
| astral-sh/ruff | `publish-playground.yml` | unsupported | — | `workflow_call` |
| astral-sh/ruff | `publish-pypi.yml` | unsupported | — | `workflow_call` |
| astral-sh/ruff | `publish-ty-playground.yml` | warnings | 23 | — |
| astral-sh/ruff | `publish-versions.yml` | unsupported | — | `inputs`, `workflow_call` |
| astral-sh/ruff | `publish-wasm.yml` | unsupported | — | `inputs`, `workflow_call` |
| astral-sh/ruff | `release.yml` | unsupported | — | `inputs`, `runs_on.unknown`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/ruff | `sync_typeshed.yaml` | unsupported | — | `runs_on.expression`, `runs_on.macos`, `runs_on.windows` |
| astral-sh/ruff | `ty-ecosystem-analyzer.yaml` | unsupported | — | `runs_on.expression` |
| astral-sh/ruff | `ty-ecosystem-report.yaml` | unsupported | — | `runs_on.expression` |
| astral-sh/ruff | `typing_conformance.yaml` | unsupported | — | `runs_on.expression` |
| astral-sh/uv | `bench.yml` | unsupported | — | `inputs`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `build-dev-binaries.yml` | unsupported | — | `inputs`, `runs_on.macos`, `runs_on.unknown`, `runs_on.windows`, `workflow_call` |
| astral-sh/uv | `build-docker.yml` | unsupported | — | `inputs`, `workflow_call` |
| astral-sh/uv | `build-release-binaries.yml` | unsupported | — | `runs_on.expression`, `runs_on.unknown`, `timeout.expression`, `workflow_call` |
| astral-sh/uv | `check-docs.yml` | unsupported | — | `workflow_call` |
| astral-sh/uv | `check-fmt.yml` | unsupported | — | `workflow_call` |
| astral-sh/uv | `check-generated-files.yml` | unsupported | — | `inputs`, `workflow_call` |
| astral-sh/uv | `check-lint.yml` | unsupported | — | `action.docker`, `inputs`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `check-lock.yml` | unsupported | — | `workflow_call` |
| astral-sh/uv | `check-publish.yml` | unsupported | — | `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `check-release.yml` | unsupported | — | `workflow_call` |
| astral-sh/uv | `check-zizmor.yml` | unsupported | — | `workflow_call` |
| astral-sh/uv | `ci.yml` | unsupported | — | `action.nested_local`, `workflow_call` |
| astral-sh/uv | `diagnose-workflow-failure.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| astral-sh/uv | `fix-bug.yml` | unsupported | — | `inputs`, `runs_on.expression`, `workflow_call` |
| astral-sh/uv | `issue-triage.yml` | unsupported | — | `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `plan.yml` | unsupported | — | `inputs`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `promote-pull-request.yml` | unsupported | — | `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `publish-crates.yml` | unsupported | — | `workflow_call` |
| astral-sh/uv | `publish-docs.yml` | unsupported | — | `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `publish-mirror.yml` | unsupported | — | `inputs`, `workflow_call` |
| astral-sh/uv | `publish-pypi.yml` | unsupported | — | `workflow_call` |
| astral-sh/uv | `publish-versions.yml` | unsupported | — | `inputs`, `workflow_call` |
| astral-sh/uv | `pull-request-conflicts.yml` | unsupported | — | `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `pull-request-labels.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| astral-sh/uv | `pull-request-security-review.yml` | unsupported | — | `workflow_call` |
| astral-sh/uv | `rebase-conflicted-pull-request.yml` | unsupported | — | `inputs`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `release-prepare.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| astral-sh/uv | `release.yml` | unsupported | — | `inputs`, `runs_on.unknown`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `reproduce-bug.yml` | unsupported | — | `inputs`, `runs_on.expression`, `workflow_call`, `workflow_dispatch.inputs` |
| astral-sh/uv | `sync-python-releases.yml` | warnings | 14 | — |
| astral-sh/uv | `sync-uv-dev.yml` | warnings | 5 | — |
| astral-sh/uv | `sync-uv-security.yml` | warnings | 6 | — |
| astral-sh/uv | `test-ecosystem.yml` | unsupported | — | `inputs`, `workflow_call` |
| astral-sh/uv | `test-integration.yml` | unsupported | — | `inputs`, `runs_on.expression`, `runs_on.macos`, `runs_on.unknown`, `runs_on.windows`, `workflow_call` |
| astral-sh/uv | `test-smoke.yml` | unsupported | — | `inputs`, `runs_on.macos`, `runs_on.unknown`, `runs_on.windows`, `workflow_call` |
| astral-sh/uv | `test-system.yml` | unsupported | — | `container.expression`, `container.options`, `inputs`, `runs_on.expression`, `runs_on.macos`, `runs_on.windows`, `workflow_call` |
| astral-sh/uv | `test-windows-trampolines.yml` | unsupported | — | `inputs`, `runs_on.expression`, `runs_on.windows`, `workflow_call` |
| astral-sh/uv | `test.yml` | unsupported | — | `inputs`, `runs_on.unknown`, `workflow_call` |
| astral-sh/uv | `update-issue-context.yml` | warnings | 33 | — |
| astral-sh/uv | `update-pull-request-parent.yml` | unsupported | — | `inputs`, `workflow_call` |
| cli/cli | `agentics-maintenance.yml` | unsupported | — | `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| cli/cli | `bump-go.yml` | warnings | 7 | — |
| cli/cli | `codeql.yml` | warnings | 13 | — |
| cli/cli | `copilot-setup-steps.yml` | warnings | 6 | — |
| cli/cli | `dependabot-triage.lock.yml` | unsupported | — | `workflow_dispatch.inputs` |
| cli/cli | `deployment.yml` | unsupported | — | `inputs`, `runs_on.macos`, `runs_on.windows`, `workflow_dispatch.inputs` |
| cli/cli | `go.yml` | unsupported | — | `runs_on.expression` |
| cli/cli | `govulncheck.yml` | warnings | 9 | — |
| cli/cli | `issue-triage.lock.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| cli/cli | `lint.yml` | warnings | 17 | — |
| cli/cli | `triage-issues.yml` | unsupported | — | `workflow_call` |
| cli/cli | `triage-pull-requests.yml` | unsupported | — | `workflow_call` |
| cli/cli | `triage-scheduled-tasks.yml` | unsupported | — | `workflow_call` |
| denoland/deno | `cargo_publish.generated.yml` | unsupported | — | `runs_on.unknown` |
| denoland/deno | `ci.generated.yml` | unsupported | — | `runs_on.expression`, `runs_on.macos`, `runs_on.windows`, `shell.powershell` |
| denoland/deno | `create_prerelease_tag.generated.yml` | warnings | 8 | — |
| denoland/deno | `ecosystem_compat_test.generated.yml` | unsupported | — | `runs_on.expression` |
| denoland/deno | `node_compat_test.generated.yml` | unsupported | — | `runs_on.expression` |
| denoland/deno | `npm_publish.generated.yml` | unsupported | — | `inputs`, `runs_on.expression`, `shell.cmd`, `workflow_dispatch.inputs` |
| denoland/deno | `post_publish.generated.yml` | clean | 11 | — |
| denoland/deno | `pr.generated.yml` | clean | 5 | — |
| denoland/deno | `promote_to_release.generated.yml` | unsupported | — | `runs_on.macos`, `runs_on.windows`, `workflow_dispatch.inputs` |
| denoland/deno | `start_release.generated.yml` | unsupported | — | `workflow_dispatch.inputs` |
| denoland/deno | `version_bump.generated.yml` | unsupported | — | `workflow_dispatch.inputs` |
| django/django | `benchmark.yml` | warnings | 10 | — |
| django/django | `check-migrations.yml` | unsupported | — | `services` |
| django/django | `check_commit_messages.yml` | warnings | 15 | — |
| django/django | `check_pr_quality.yml` | warnings | 7 | — |
| django/django | `coverage_comment.yml` | warnings | 4 | — |
| django/django | `coverage_tests.yml` | unsupported | — | `services` |
| django/django | `docs.yml` | warnings | 11 | — |
| django/django | `labels.yml` | warnings | 3 | — |
| django/django | `linters.yml` | warnings | 35 | — |
| django/django | `new_contributor_pr.yml` | unsupported | — | `action.docker` |
| django/django | `playwright.yml` | unsupported | — | `services` |
| django/django | `postgis.yml` | warnings | 12 | — |
| django/django | `python_matrix.yml` | warnings | 15 | — |
| django/django | `schedule_tests.yml` | unsupported | — | `runs_on.windows`, `services` |
| django/django | `schedules.yml` | warnings | 3 | — |
| django/django | `screenshots.yml` | warnings | 17 | — |
| django/django | `tests.yml` | unsupported | — | `runs_on.windows` |
| facebook/react | `compiler_discord_notify.yml` | unsupported | — | `workflow_call` |
| facebook/react | `compiler_playground.yml` | warnings | 17 | — |
| facebook/react | `compiler_prereleases.yml` | unsupported | — | `inputs`, `workflow_call` |
| facebook/react | `compiler_prereleases_manual.yml` | unsupported | — | `workflow_call`, `workflow_dispatch.inputs` |
| facebook/react | `compiler_prereleases_nightly.yml` | unsupported | — | `workflow_call` |
| facebook/react | `compiler_rust.yml` | warnings | 30 | — |
| facebook/react | `compiler_typescript.yml` | warnings | 36 | — |
| facebook/react | `devtools_discord_notify.yml` | unsupported | — | `workflow_call` |
| facebook/react | `devtools_regression_tests.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| facebook/react | `runtime_build_and_test.yml` | unsupported | — | `step.background` |
| facebook/react | `runtime_commit_artifacts.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| facebook/react | `runtime_discord_notify.yml` | unsupported | — | `workflow_call` |
| facebook/react | `runtime_eslint_plugin_e2e.yml` | warnings | 14 | — |
| facebook/react | `runtime_fuzz_tests.yml` | warnings | 8 | — |
| facebook/react | `runtime_release_from_ci.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| facebook/react | `runtime_sizebot_comment.yml` | warnings | 10 | — |
| facebook/react | `shared_check_maintainer.yml` | unsupported | — | `inputs`, `workflow_call` |
| facebook/react | `shared_cleanup_merged_branch_caches.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| facebook/react | `shared_cleanup_stale_branch_caches.yml` | warnings | 3 | — |
| facebook/react | `shared_close_direct_sync_branch_prs.yml` | warnings | 3 | — |
| facebook/react | `shared_label_core_team_prs.yml` | unsupported | — | `workflow_call` |
| facebook/react | `shared_lint.yml` | warnings | 44 | — |
| facebook/react | `shared_stale.yml` | warnings | 3 | — |
| hashicorp/terraform | `backport.yml` | warnings | 3 | — |
| hashicorp/terraform | `build-terraform-cli.yml` | unsupported | — | `inputs`, `runs_on.expression`, `workflow_call` |
| hashicorp/terraform | `build.yml` | unsupported | — | `action.docker`, `runs_on.expression`, `shell.cmd`, `shell.powershell`, `workflow_call` |
| hashicorp/terraform | `changelog-validation.yml` | warnings | 5 | — |
| hashicorp/terraform | `checks.yml` | warnings | 43 | — |
| hashicorp/terraform | `enforce-changelog.yml` | warnings | 6 | — |
| hashicorp/terraform | `equivalence-test-diff.yml` | warnings | 12 | — |
| hashicorp/terraform | `equivalence-test-manual-update.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| hashicorp/terraform | `equivalence-test-update.yml` | unsupported | — | `inputs` |
| hashicorp/terraform | `issue-comment-created.yml` | warnings | 3 | — |
| hashicorp/terraform | `lock.yml` | warnings | 3 | — |
| nodejs/node | `auto-start-ci.yml` | warnings | 10 | — |
| nodejs/node | `benchmark.yml` | unsupported | — | `inputs`, `runs_on.expression`, `workflow_dispatch.inputs` |
| nodejs/node | `build-shared.yml` | unsupported | — | `inputs`, `runs_on.expression`, `workflow_call` |
| nodejs/node | `build-tarball.yml` | warnings | 21 | — |
| nodejs/node | `close-stalled.yml` | warnings | 3 | — |
| nodejs/node | `codeql.yml` | warnings | 9 | — |
| nodejs/node | `comment-labeled.yml` | warnings | 9 | — |
| nodejs/node | `commit-lint.yml` | warnings | 9 | — |
| nodejs/node | `commit-queue.yml` | warnings | 13 | — |
| nodejs/node | `coverage-linux-without-intl.yml` | warnings | 24 | — |
| nodejs/node | `coverage-linux.yml` | warnings | 24 | — |
| nodejs/node | `coverage-windows.yml` | unsupported | — | `runs_on.windows` |
| nodejs/node | `create-release-proposal.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| nodejs/node | `daily-wpt-fyi.yml` | warnings | 36 | — |
| nodejs/node | `daily.yml` | warnings | 7 | — |
| nodejs/node | `doc.yml` | warnings | 9 | — |
| nodejs/node | `find-inactive-collaborators.yml` | warnings | 8 | — |
| nodejs/node | `find-inactive-tsc.yml` | warnings | 10 | — |
| nodejs/node | `label-flaky-test-issue.yml` | warnings | 4 | — |
| nodejs/node | `label-pr.yml` | warnings | 3 | — |
| nodejs/node | `license-builder.yml` | warnings | 6 | — |
| nodejs/node | `lint-release-proposal.yml` | warnings | 9 | — |
| nodejs/node | `linters.yml` | unsupported | — | `action.docker` |
| nodejs/node | `major-release.yml` | warnings | 6 | — |
| nodejs/node | `nix-changes-comment.yml` | warnings | 5 | — |
| nodejs/node | `nix-changes.yml` | unsupported | — | `runs_on.expression` |
| nodejs/node | `notify-on-push.yml` | unsupported | — | `action.docker` |
| nodejs/node | `notify-on-review-wanted.yml` | unsupported | — | `action.docker` |
| nodejs/node | `post-release.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| nodejs/node | `scorecard.yml` | unsupported | — | `action.docker` |
| nodejs/node | `stale.yml` | warnings | 3 | — |
| nodejs/node | `stress-test.yml` | unsupported | — | `action.local_missing`, `inputs`, `runs_on.expression`, `workflow_dispatch.inputs` |
| nodejs/node | `test-internet.yml` | warnings | 11 | — |
| nodejs/node | `test-linux-quic.yml` | unsupported | — | `action.local_missing` |
| nodejs/node | `test-linux.yml` | unsupported | — | `action.local_missing`, `runs_on.expression` |
| nodejs/node | `test-macos.yml` | unsupported | — | `runs_on.macos` |
| nodejs/node | `test-shared.yml` | unsupported | — | `workflow_call` |
| nodejs/node | `timezone-update.yml` | warnings | 12 | — |
| nodejs/node | `tools.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| nodejs/node | `update-openssl.yml` | warnings | 8 | — |
| nodejs/node | `update-v8.yml` | warnings | 12 | — |
| nodejs/node | `update-wpt.yml` | unsupported | — | `workflow_dispatch.inputs` |
| ohmyzsh/ohmyzsh | `dependencies.yml` | warnings | 12 | — |
| ohmyzsh/ohmyzsh | `installer.yml` | unsupported | — | `runs_on.expression` |
| ohmyzsh/ohmyzsh | `main.yml` | warnings | 9 | — |
| ohmyzsh/ohmyzsh | `project.yml` | warnings | 11 | — |
| ohmyzsh/ohmyzsh | `scorecard.yml` | unsupported | — | `action.docker` |
| pola-rs/polars | `benchmark-remote.yml` | unsupported | — | `runs_on.unknown` |
| pola-rs/polars | `benchmark.yml` | warnings | 21 | — |
| pola-rs/polars | `changes-dsl-labeler.yml` | warnings | 6 | — |
| pola-rs/polars | `clear-caches.yml` | clean | 5 | — |
| pola-rs/polars | `docs-global.yml` | unsupported | — | `action.docker`, `runs_on.macos` |
| pola-rs/polars | `docs-python.yml` | unsupported | — | `workflow_dispatch.inputs` |
| pola-rs/polars | `docs-rust.yml` | warnings | 11 | — |
| pola-rs/polars | `issue-labeler.yml` | warnings | 3 | — |
| pola-rs/polars | `lint-global.yml` | warnings | 10 | — |
| pola-rs/polars | `lint-python.yml` | warnings | 18 | — |
| pola-rs/polars | `lint-rust.yml` | warnings | 38 | — |
| pola-rs/polars | `pr-labeler.yml` | warnings | 5 | — |
| pola-rs/polars | `release-drafter.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| pola-rs/polars | `release-python.yml` | unsupported | — | `action.nested_local`, `inputs`, `runs_on.expression`, `shell.powershell`, `workflow_dispatch.inputs` |
| pola-rs/polars | `release-rust.yml` | clean | 4 | — |
| pola-rs/polars | `test-bytecode-parser.yml` | warnings | 8 | — |
| pola-rs/polars | `test-coverage.yml` | unsupported | — | `runs_on.macos` |
| pola-rs/polars | `test-pyodide.yml` | warnings | 8 | — |
| pola-rs/polars | `test-python.yml` | unsupported | — | `runs_on.expression`, `shell.powershell` |
| pola-rs/polars | `test-rust.yml` | unsupported | — | `runs_on.expression` |
| prometheus/prometheus | `approve-workflows.yml` | warnings | 3 | — |
| prometheus/prometheus | `automerge-dependabot.yml` | warnings | 4 | — |
| prometheus/prometheus | `buf-lint.yml` | warnings | 7 | — |
| prometheus/prometheus | `buf.yml` | warnings | 8 | — |
| prometheus/prometheus | `check_release_notes.yml` | warnings | 5 | — |
| prometheus/prometheus | `ci.yml` | unsupported | — | `container.expression`, `runs_on.windows`, `shell.powershell`, `workflow_call` |
| prometheus/prometheus | `codeql-analysis.yml` | unsupported | — | `workflow_call` |
| prometheus/prometheus | `container_description.yml` | unsupported | — | `action.docker` |
| prometheus/prometheus | `fuzzing.yml` | unsupported | — | `workflow_call` |
| prometheus/prometheus | `govulncheck.yml` | warnings | 8 | — |
| prometheus/prometheus | `lock.yml` | warnings | 3 | — |
| prometheus/prometheus | `prombench.yml` | unsupported | — | `action.docker` |
| prometheus/prometheus | `repo_sync.yml` | warnings | 5 | — |
| prometheus/prometheus | `scorecards.yml` | unsupported | — | `action.docker` |
| prometheus/prometheus | `stale.yml` | warnings | 3 | — |
| python/cpython | `add-issue-header.yml` | warnings | 3 | — |
| python/cpython | `build.yml` | unsupported | — | `runs_on.expression`, `runs_on.macos`, `workflow_call` |
| python/cpython | `jit.yml` | unsupported | — | `yaml.anchors` |
| python/cpython | `lint.yml` | warnings | 6 | — |
| python/cpython | `mypy.yml` | unsupported | — | `yaml.multiline_flow` |
| python/cpython | `new-bugs-announce-notifier.yml` | warnings | 6 | — |
| python/cpython | `require-pr-label.yml` | warnings | 8 | — |
| python/cpython | `reusable-check-c-api-docs.yml` | unsupported | — | `workflow_call` |
| python/cpython | `reusable-check-html-ids.yml` | unsupported | — | `workflow_call` |
| python/cpython | `reusable-cifuzz.yml` | unsupported | — | `action.docker`, `inputs`, `workflow_call` |
| python/cpython | `reusable-context.yml` | unsupported | — | `workflow_call` |
| python/cpython | `reusable-docs.yml` | unsupported | — | `workflow_call` |
| python/cpython | `reusable-emscripten.yml` | unsupported | — | `workflow_call` |
| python/cpython | `reusable-install.yml` | unsupported | — | `runs_on.unknown`, `workflow_call` |
| python/cpython | `reusable-macos.yml` | unsupported | — | `inputs`, `runs_on.expression`, `workflow_call` |
| python/cpython | `reusable-san.yml` | unsupported | — | `inputs`, `workflow_call` |
| python/cpython | `reusable-ubuntu.yml` | unsupported | — | `inputs`, `runs_on.expression`, `workflow_call` |
| python/cpython | `reusable-wasi.yml` | unsupported | — | `runs_on.unknown`, `workflow_call` |
| python/cpython | `reusable-windows.yml` | unsupported | — | `inputs`, `runs_on.expression`, `workflow_call` |
| python/cpython | `stale.yml` | warnings | 3 | — |
| python/cpython | `tail-call.yml` | unsupported | — | `yaml.anchors` |
| python/cpython | `verify-ensurepip-wheels.yml` | warnings | 7 | — |
| python/cpython | `verify-expat.yml` | warnings | 5 | — |
| rails/rails | `check-markdown-api.yml` | warnings | 6 | — |
| rails/rails | `devcontainer-shellcheck.yml` | warnings | 5 | — |
| rails/rails | `devcontainer-smoke-test.yml` | warnings | 23 | — |
| rails/rails | `labeler.yml` | warnings | 3 | — |
| rails/rails | `more-info-needed.yml` | warnings | 3 | — |
| rails/rails | `rail_inspector.yml` | warnings | 7 | — |
| rails/rails | `rails-new-docker.yml` | warnings | 13 | — |
| rails/rails | `rails_releaser_tests.yml` | warnings | 7 | — |
| rails/rails | `release.yml` | warnings | 12 | — |
| rails/rails | `stale.yml` | warnings | 3 | — |
| rust-lang/cargo | `audit.yml` | unsupported | — | `action.docker` |
| rust-lang/cargo | `contrib.yml` | warnings | 13 | — |
| rust-lang/cargo | `main.yml` | unsupported | — | `runs_on.expression` |
| rust-lang/cargo | `release.yml` | warnings | 5 | — |
| serde-rs/serde | `ci.yml` | unsupported | — | `runs_on.expression`, `runs_on.windows` |
| sharkdp/bat | `CICD.yml` | unsupported | — | `runs_on.expression`, `shell.powershell` |
| sharkdp/bat | `require-changelog-for-PRs.yml` | clean | 7 | — |
| tailwindlabs/tailwindcss | `ci.yml` | unsupported | — | `runs_on.expression` |
| tailwindlabs/tailwindcss | `integration-tests.yml` | unsupported | — | `runs_on.expression` |
| tailwindlabs/tailwindcss | `prepare-release.yml` | unsupported | — | `inputs`, `runs_on.expression`, `runs_on.macos`, `workflow_dispatch.inputs` |
| tailwindlabs/tailwindcss | `release.yml` | unsupported | — | `runs_on.expression`, `runs_on.macos`, `workflow_dispatch.inputs` |
| tokio-rs/tokio | `audit.yml` | unsupported | — | `action.docker` |
| tokio-rs/tokio | `ci.yml` | unsupported | — | `runs_on.expression`, `workflow_call` |
| tokio-rs/tokio | `labeler.yml` | warnings | 3 | — |
| tokio-rs/tokio | `loom.yml` | warnings | 120 | — |
| tokio-rs/tokio | `pr-audit.yml` | unsupported | — | `action.docker` |
| tokio-rs/tokio | `stress-test.yml` | warnings | 23 | — |
| tokio-rs/tokio | `uring-kernel-version-test.yml` | unsupported | — | `inputs`, `workflow_call` |
| vercel/next.js | `automated_code_review.yml` | unsupported | — | `action.remote` |
| vercel/next.js | `build_and_deploy.yml` | unsupported | — | `runs_on.expression`, `runs_on.unknown`, `shell.powershell` |
| vercel/next.js | `build_and_test.yml` | unsupported | — | `workflow_call` |
| vercel/next.js | `build_reusable.yml` | unsupported | — | `inputs`, `runs_on.expression`, `shell.powershell`, `step.background`, `timeout.expression`, `workflow_call` |
| vercel/next.js | `code_freeze.yml` | unsupported | — | `workflow_dispatch.inputs` |
| vercel/next.js | `create_release_branch.yml` | unsupported | — | `workflow_dispatch.inputs` |
| vercel/next.js | `integration_tests_reusable.yml` | unsupported | — | `inputs`, `runs_on.unknown`, `workflow_call` |
| vercel/next.js | `issue_lock.yml` | warnings | 3 | — |
| vercel/next.js | `issue_reopen.yml` | warnings | 5 | — |
| vercel/next.js | `issue_stale.yml` | clean | 6 | — |
| vercel/next.js | `issue_wrong_template.yml` | clean | 8 | — |
| vercel/next.js | `popular.yml` | clean | 10 | — |
| vercel/next.js | `pr_ci_comment.yml` | warnings | 6 | — |
| vercel/next.js | `pr_stack_optimizer.yml` | unsupported | — | `workflow_call` |
| vercel/next.js | `pull_request_auto_label.yml` | warnings | 3 | — |
| vercel/next.js | `pull_request_stats.yml` | unsupported | — | `action.docker`, `runs_on.unknown`, `workflow_call` |
| vercel/next.js | `release-next-rspack.yml` | unsupported | — | `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| vercel/next.js | `retry_deploy_test.yml` | warnings | 6 | — |
| vercel/next.js | `retry_test.yml` | warnings | 8 | — |
| vercel/next.js | `rspack-nextjs-build-integration-tests.yml` | unsupported | — | `workflow_call` |
| vercel/next.js | `rspack-nextjs-dev-integration-tests.yml` | unsupported | — | `workflow_call` |
| vercel/next.js | `rspack-update-tests-manifest.yml` | clean | 24 | — |
| vercel/next.js | `sync_backport_canary_release.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| vercel/next.js | `test-turbopack-rust-bench-test.yml` | unsupported | — | `inputs`, `runs_on.expression`, `shell.powershell`, `workflow_call` |
| vercel/next.js | `test_e2e_deploy_release.yml` | unsupported | — | `inputs`, `workflow_call`, `workflow_dispatch.inputs` |
| vercel/next.js | `test_e2e_project_reset_cron.yml` | clean | 9 | — |
| vercel/next.js | `test_examples.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| vercel/next.js | `triage.yml` | warnings | 4 | — |
| vercel/next.js | `trigger_release.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| vercel/next.js | `turbopack-benchmark.yml` | unsupported | — | `runs_on.unknown`, `shell.powershell` |
| vercel/next.js | `turbopack-nextjs-build-integration-tests.yml` | unsupported | — | `workflow_call` |
| vercel/next.js | `turbopack-nextjs-dev-integration-tests.yml` | unsupported | — | `workflow_call` |
| vercel/next.js | `turbopack-update-tests-manifest.yml` | clean | 24 | — |
| vercel/next.js | `update_fonts_data.yml` | clean | 12 | — |
| vercel/next.js | `update_react.yml` | unsupported | — | `inputs`, `workflow_dispatch.inputs` |
| vercel/next.js | `update_react_poller.yml` | warnings | 8 | — |
| vercel/next.js | `upload-tests-manifest.yml` | clean | 9 | — |
| vercel/next.js | `upload_preview_tarballs.yml` | warnings | 13 | — |
