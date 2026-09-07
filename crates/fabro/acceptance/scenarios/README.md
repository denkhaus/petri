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

Families: `code-review`, `security-review`, `fix-ci`, `implement`,
`interview`, `provider-faults`, `routing`, and `backend` (the
backend-matrix cases). The family's minimum scenarios
and critical outcomes are the phase 4 table of
`.ai/plans/fabro-black-box-e2e.md`; `../CONTRACT.md` records which
obligation each scenario satisfies and which remain blocked.

## Running

`mise run test:fabro:blackbox` runs the routine subset: every scenario cell
of `matrix.json` whose `status` is `planned` on the host backend, plus the
Docker cells when a daemon is reachable. The bundles must be fetched first
(`scripts/corpus-fetch-fabro-bundles.sh`); a missing bundle skips its
scenarios unless `PETRI_REQUIRE_FABRO_BUNDLES=1`, which fails them.

Each cell is one Nextest test in `crates/petri/cli/tests/fabro_scenarios_blackbox.rs`.
The test stages the fixture repository under a fresh directory, starts the
twins, runs `petri run`, reads the run back through `petri inspect --json`,
and applies the scenario's `expect` block through
`crates/petri/cli/tests/support/fabro/scenario.rs`.

## Coverage report

Every test writes its cell's result to `$PETRI_FABRO_COVERAGE_DIR`
(default `target/fabro-coverage/results/`). `scripts/fabro-coverage-report.py`
merges those results with `matrix.json` into `coverage.json`: for every
required cell one of `passed`, `failed`, `blocked`, `excluded`, or
`missing` (a required cell no test reported, which counts as failed). CI
publishes that file; a filtered-out or skipped case is therefore visible.

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
