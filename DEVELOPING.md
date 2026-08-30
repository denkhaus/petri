# Developing Petri

## Setup

Install the stable Rust toolchain with the `rustfmt` and `clippy` components.
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

Prepare the pinned Rust Style Guide before Rust work:

```sh
bin/style-guides prepare
```

## Common tasks

| Command | Purpose |
| --- | --- |
| `cargo fmt --all` | Format Rust code |
| `cargo fmt --all --check` | Check formatting without changing files |
| `cargo clippy --all-targets --all-features -- -D warnings` | Run Clippy |
| `cargo test --workspace --all-features` | Run the routine test suite |
| `cargo build --workspace --release` | Build the workspace in release mode |
| `./scripts/preflight.sh` | Run the complete local verification gate |

Run `./scripts/preflight.sh` before opening a pull request.

## Rust policy

Petri follows the pinned Brynary Rust Style Guide. Run
`bin/style-guides prepare`, then read
`.ai/style-guides/rust-style-guide/SKILL.md` before changing Rust code,
configuration, project structure, or tests.

Petri uses Rust 2024. Tokio owns asynchronous subprocesses, timers, networking,
and orchestration. Keep parsing, validation, graph transformations, and the
engine state machine synchronous unless a real I/O boundary requires async.

The workspace contains internal crates, a distribution library, a CLI, and test
support. Treat shared in-repository crates as application code unless the
project explicitly gives an API to independent consumers.

## Project structure

`README.md` maps Petri's design documents to the workspace and its tests. Keep
crate dependencies consistent with the layering rules described there.
`crates/petri/lib/tests/layering.rs` enforces the dependency graph.

## Tests and the compatibility corpus

The routine suite runs without Docker or the corpus. It reports skips when those
resources are absent.

CI sets `PETRI_REQUIRE_DOCKER=1` on Linux and `PETRI_REQUIRE_CORPUS=1` on all
runners. These variables turn an unexpected skip into a failure. Do not set
them for ordinary local work unless both resources are available.

The corpus contains third-party repositories and is not committed. The corpus
run battery does not receive a GitHub token, so workflows from the corpus cannot
mutate repositories through the GitHub API.

## Continuous integration

Pull requests and pushes to `main` run formatting, Clippy, and the full test
suite on Linux and macOS. A separate Linux job builds the workspace in release
mode. Linux also runs the Docker-backed acceptance tests and checks that Petri
leaves no containers behind.
