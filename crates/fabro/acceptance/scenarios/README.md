# Black box scenarios

The complete-workflow scenarios of the Fabro black box battery (black box
plan phases 3 to 5). Each scenario runs a pinned bundle from
`../bundles.lock.json` through the shipped `petri` binary with provider
twins on loopback, real shell, file and Git tools, and a scripted
interviewer, and asserts the final context, files, side effects, provider
requests, interviews, and lifecycle it declared before the run.

## Layout

```
scenarios/
  SCHEMA.md                     the scenario format, version 1
  README.md                     this file
  matrix.json                   every required (scenario, backend, agent) cell
  <family>/<name>.scenario.json one scenario
  <family>/fixtures/…           files a scenario's fixture or services reference
  <family>/expected/…           expected file contents a scenario references
  parallel-results/             task 3's regression (its own CONTRACT.md and Fabro capture)
```

Families: `code-review`, `security-review`, `implement`, `interview`,
`provider-faults`, `routing`, and `backend` (the backend-matrix cases). The
`fix-ci` bundle is excluded by an owner decision of 2026-09-07; `../CONTRACT.md`
records the reason and where its behavior is covered instead. The family's minimum scenarios
and critical outcomes are the phase 4 table of
`.ai/plans/fabro-black-box-e2e.md`; `../CONTRACT.md` records which
obligation each scenario satisfies and which remain blocked.

## Coverage states

`matrix.json` declares each cell `planned`, `blocked` or `excluded`, and the
report resolves it to one state: `passed`, `external` (a cell verified by a
test in another suite, named in the report), `failed`, `skipped` (a gated
resource was missing), `missing` (a planned cell no test reported, which
counts as failed), `blocked` (a required cell that cannot run yet, with its
reason) or `excluded` (not required, with its reason).

## Running

`mise run test:fabro:blackbox` runs every planned cell of `matrix.json` and
then the coverage report, allowing a skipped cell (a machine with no Docker).
`mise run test:fabro:blackbox:strict` requires Docker
(`PETRI_REQUIRE_DOCKER=1`) and the fetched bundles
(`PETRI_REQUIRE_FABRO_BUNDLES=1`) and fails on any skipped cell; the nightly
gate runs it, so the repeated container work stays out of the routine
subset. The bundles must be fetched first
(`scripts/corpus-fetch-fabro-bundles.sh`); a missing bundle skips its
scenarios in the routine task and fails them in the strict one.

Each cell is one Nextest test in `crates/petri/cli/tests/fabro_scenarios_blackbox.rs`.
The test stages the fixture repository under a fresh directory, starts the
twins, runs `petri run`, reads the run back through `petri inspect --json`,
and applies the scenario's `expect` block through
`crates/petri/cli/tests/support/fabro/scenario.rs`.

## Coverage report

Every test writes its cell's result to `$PETRI_FABRO_COVERAGE_DIR`
(default `target/fabro-coverage/results/`) when it starts, as a failure, and
rewrites it as a pass only when every expectation held, so a panic or a
killed process leaves a failure on record. `scripts/fabro-coverage-report.py`
merges those results with `matrix.json` into `coverage.json` and prints the
table; its exit code is nonzero when a planned cell is not accounted for. CI
publishes that file.

## Writing a scenario

1. Pick the bundle and copy its `bundle_hash` from the lock file.
2. Write the fixture: the smallest repository the helpers accept, committed
   in order, with local remotes when the workflow pushes.
3. Script every model turn the run will make, matched on text that only
   that request carries (a job id, a candidate id, a tool result marker).
4. Write `expect` completely, before running: every row, including the
   empty ones.
5. Add the cell to `matrix.json` and the test to
   `fabro_scenarios_blackbox.rs`.

A scenario never patches the bundle's graph, prompts, or helper scripts. It
adapts only the declared environment (`fixture.env`, `fixture.bin`) and the
fixture data.
