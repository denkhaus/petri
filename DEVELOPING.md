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

Run `mise run check` before opening a pull request.

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

## Crate policy

The external distribution surface is:

- `crates/petri/lib`, the `petri` library that re-exports the supported API;
- `crates/petri/cli`, the shipped `petri` executable.

All crates under `crates/core/` and `crates/github/` are internal components or
test support. Their manifests set `publish = false`. Treat their public items as
in-repository APIs unless a later project decision gives independent consumers
a direct contract.

Do not publish either distribution crate manually. Crates.io publication and
binary release automation require a separate release decision.

## Project structure

`README.md` maps Petri's design documents to the workspace and its tests. Keep
crate dependencies consistent with the layering rules described there.
`crates/petri/lib/tests/layering.rs` enforces the dependency graph.

## Tests and the compatibility corpus

The routine suite runs without Docker or the corpus. It reports skips when those
resources are absent.

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
