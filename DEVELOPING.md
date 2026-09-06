# Developing Petri

## Setup

Install [Mise](https://mise.jdx.dev/), then install the locked tools and prepare
the pinned Rust Style Guide:

```sh
mise trust
mise install --locked --jobs=1
mise run setup
```

Install Docker to run the container-backed tests. Tests that need Docker skip
when no daemon is reachable.

The GitHub Actions compatibility corpus is optional for normal local work. To
prepare it, install and authenticate the GitHub CLI, then run:

```sh
scripts/corpus-fetch.sh
scripts/corpus-fetch-actions.sh
```

The corpus uses the commits in `crates/github/corpus-pins.txt`. Do not use
`--repin` during a routine fetch.

## Private dependencies

The workspace pins sandbox-driver, Pebble, and lithos-llm by Git revision.
Local Cargo builds use your Git SSH credentials. Each account needs read access
to all three repositories. Use a local, untracked Cargo `[patch]` config when
working across sibling checkouts; commit a pushed revision in `Cargo.toml` and
regenerate `Cargo.lock` before sharing the integration.

CI, nightly, and release workflows use `.github/actions/private-dependencies`.
The `sandbox-driver-read` GitHub environment must contain these secrets:

- `SANDBOX_DRIVER_DEPLOY_KEY`: read-only deploy key on `lithoscomputer/sandbox-driver`.
- `PEBBLE_DEPLOY_KEY`: read-only deploy key on `lithoscomputer/pebble`.
- `LITHOS_LLM_DEPLOY_KEY`: read-only deploy key on `lithoscomputer/lithos-llm`.

Use a different key pair for each repository. The action selects each key with
an SSH host alias and checks GitHub's pinned host key. Each workflow removes
the temporary credentials when its job finishes.

Native Pebble tests use a scripted model and real execution scopes. They need
no provider credentials. `crates/fabro/steps/tests/pebble.rs` covers tool
execution, output repair, model selection, accounting, steering, cancellation,
bounded output capture, and the Pebble environment contract on Host and Docker.

## Common tasks

| Command | Purpose |
| --- | --- |
| `mise run dev` | Build and run the Petri CLI |
| `mise run fmt` | Format Rust code |
| `mise run fmt:check` | Check formatting without changing files |
| `mise run lint` | Run Clippy with warnings denied |
| `mise run test` | Run the routine suite with Nextest, then run doctests |
| `mise run check:msrv` | Check all targets with Rust 1.89 |
| `mise run check` | Run the complete routine verification gate |
| `mise run check:nightly` | Run the extended verification gate |
| `mise run release <target> <version>` | Build a native release archive |

Run `mise run check` before opening a pull request.

## Diagnostics

Set `PETRI_LOG` to see tracing output on stderr. The default is `warn`. Use
`PETRI_LOG=debug` for per-step and per-external-call detail:

```sh
PETRI_LOG=debug mise run dev -- run workflow.yml
```

`PETRI_LOG` takes any `tracing_subscriber` filter directive. Diagnostics never
mix with command output on stdout. Tracing fields carry only structural data;
secrets, step output, and environment values are never captured.

## Rust policy

Petri follows the pinned Brynary Rust Style Guide. Run `mise run setup`, then read
`.ai/style-guides/rust-style-guide/SKILL.md` before changing Rust code,
configuration, project structure, or tests.

Petri uses Rust 2024 and supports Rust 1.89 or newer. Mise pins the minimum
compiler, the development compiler, and the nightly formatter. Tokio owns
asynchronous subprocesses, timers, networking, and orchestration. Keep parsing,
validation, graph transformations, and the engine state machine synchronous
unless a real I/O boundary requires async.

The `petri-cli` crate owns Tokio runtime creation. Library crates can expose
Tokio-based async APIs, but they must not create a process-wide runtime.

## Lints and unsafe code

The root `Cargo.toml` holds the workspace lint tables, and every member crate
opts in with `[lints] workspace = true`. Workspace lints are not inherited
automatically, so a new crate must add that table or the policy does nothing
for it. The root `clippy.toml` allows `unwrap` in tests and on
`std::sync::LockResult`, and nowhere else.

`mise run lint` runs the whole policy with warnings denied. It is the source of
truth. Repair a diagnostic with a small, behavior-preserving code change first.
When the code is intentionally different from the policy, put
`#[expect(LINT, reason = "...")]` on the narrowest item or expression and state
the real constraint in the reason. Do not add a workspace-wide exemption to
silence one site.

Project-written unsafe code is denied by default: `unsafe_code = "deny"` in the
workspace lint table. The host executor module of `petri-executor-sandbox`
(`src/host.rs`) is the only exception. It is limited to POSIX process control
(`killpg`) and the macOS `libproc` queries the host executor uses to observe a
process group without signalling it. The module takes the exception with a
reasoned `#![allow(unsafe_code, ...)]` at its top.

Every unsafe operation carries an adjacent `SAFETY:` comment. The comment must
prove the preconditions that operation relies on: pointer validity, initialized
storage, buffer size, exclusive borrow, and the operating system's own
contract. Keep each unsafe block around only the operation that requires it.
Ordinary filtering and iteration stay outside. Adding unsafe code to any other
crate needs a project decision, not a local attribute.

## Crate policy

The external distribution surface is:

- `crates/petri/lib`, the `petri` library that re-exports the supported API;
- `crates/petri/cli`, the shipped `petri` executable.

All crates under `crates/core/` and `crates/github/` are internal components or
test support. Their manifests set `publish = false`. Treat their public items as
in-repository APIs unless a later project decision gives independent consumers
a direct contract.

Do not publish either distribution crate to crates.io. The binary release
workflow packages the `petri` executable. It does not publish a crate.

## Project structure

`README.md` maps Petri's design documents to the workspace and its tests. Keep
crate dependencies consistent with the layering rules described there.
`crates/petri/lib/tests/layering.rs` enforces the dependency graph.

## Tests and the compatibility corpus

The routine suite runs without Docker or the corpus. It reports skips when those
resources are absent.

Container scopes run through the `sandbox-driver-docker` plugin. `mise run
plugins:build` installs it from the pinned sandbox-driver revision under
`target/plugins/bin`; the test tasks depend on it and set
`PETRI_SANDBOX_DOCKER_PLUGIN` to that path. To test against another build of the
plugin, set the variable yourself. A plugin this build does not pin runs only in
dev mode, which debug builds turn on; a release build needs
`PETRI_SANDBOX_PLUGIN_DEV=1` or `--sandbox-plugin-dev`.

To build plugins from the sibling sandbox-driver checkout during coordinated
development, use:

```sh
SANDBOX_DRIVER_SOURCE=../sandbox-driver mise run plugins:build
```

Both local and pinned Git builds install the provider packages directly.
Each package builds its same-named plugin executable.

Nextest is the normal test runner. `mise run test` also uses Cargo to run
doctests, which Nextest does not run.

CI sets `PETRI_REQUIRE_DOCKER=1` on Linux and `PETRI_REQUIRE_CORPUS=1` on all
runners. These variables turn an unexpected skip into a failure. Do not set
them for ordinary local work unless both resources are available.

The corpus contains third-party repositories and is not committed. The corpus
run battery does not receive a GitHub token, so workflows from the corpus cannot
mutate repositories through the GitHub API.

## Continuous integration

Pull requests and pushes to `main` run the routine verification gate on fixed
Linux and macOS runners. The gate checks formatting, Clippy, GitHub Actions,
whitespace, the full test suite, and a release build. Linux also runs the
Docker-backed acceptance tests and checks that Petri leaves no containers
behind.

The scheduled Nightly workflow also checks the Rust 1.89 compiler floor and
runs the full test suite in release mode. You can start it manually from GitHub
Actions. Nightly includes Linux arm64 coverage. Keep arm64 in Nightly until its
Docker and corpus runs are reliable enough for the routine gate.

## Releases

A `v*` tag builds native `petri` archives for macOS arm64, Linux x86_64, and
Linux arm64. The workflow adds a SHA-256 file for each archive and creates a
draft GitHub release. Review and publish the draft manually.

To test packaging locally, run the release task with your native Rust target
and a version that starts with `v`, for example:

```sh
mise run release aarch64-apple-darwin v0.1.0-test
```

The task writes ignored artifacts under `dist-release/`. It refuses to build a
target that does not match the host runner.
