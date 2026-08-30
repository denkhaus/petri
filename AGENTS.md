# Repository Instructions

## Project purpose

Petri is a Rust workflow engine. It runs a token-flow graph with explicit
routing and supports native workflows and GitHub Actions workflows.

Use these documents as the authoritative design sources:

- `engine-spec.md` defines the engine and intermediate representation.
- `frontend-handoff.md` defines the frontend contract.
- `crates/core/frontend-native/FORMAT.md` defines the native workflow format.
- `README.md` maps the design to the workspace and test suites.

## Rust style

Before changing Rust code, configuration, project structure, or tests:

1. Run `bin/style-guides prepare` when that command is available.
2. Read `.ai/style-guides/rust-style-guide/SKILL.md` completely.
3. Read each workflow and policy page that the skill routes for the task.

Project requirements and accepted architecture decisions override general
style-guide defaults.

## Repository tasks

- Use `./scripts/preflight.sh` for the complete local verification gate.
- Use `cargo fmt --all` to format Rust.
- Use `cargo clippy --all-targets --all-features -- -D warnings` to lint Rust.
- Use `cargo test --workspace --all-features` to run the routine test suite.
- Use `scripts/corpus-fetch.sh` and `scripts/corpus-fetch-actions.sh` to prepare
  the GitHub Actions compatibility corpus.

Docker tests skip when no Docker daemon is available. CI requires Docker tests
on Linux and requires the compatibility corpus on all runners.

## Safety

- Never install packages less than 24 hours old.
- Never force push, including with `--force-with-lease`.
- Never amend commits. Create a new commit instead.

## Working documents

Save plans under `.ai/plans/` and reviews under `.ai/reviews/`.

Use plain language. Prefer short, direct sentences and established project
terms.
