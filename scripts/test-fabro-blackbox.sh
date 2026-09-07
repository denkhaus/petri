#!/usr/bin/env bash
# Run the required Fabro black box scenarios on the shipped `petri` binary,
# write one evidence record per scenario into a run-scoped directory, and
# write the coverage report for the run.
#
#   scripts/test-fabro-blackbox.sh [nextest args...]
#
# The scenario set is the same locally and in CI: every `petri-cli` test
# binary named `fabro_*blackbox`, plus `standalone` and `fabro_cli`. CI's
# routine job runs the whole workspace suite, a superset, with the same
# evidence directory and the same coverage report.
#
# Environment:
#   PETRI_EVIDENCE_DIR     where records go; default target/fabro-evidence/<run id>
#                          (target/fabro-evidence/latest links to the newest default run)
#   PETRI_EVIDENCE_RUN     the run id used for the default directory
#   PETRI_SCENARIO_MATRIX  the scenario matrix the coverage report reads;
#                          default crates/fabro/acceptance/scenarios/matrix.json
#   PETRI_FABRO_COVERAGE_DIR where the scenario tests write per-cell results;
#                          default <evidence dir>/cells
#   PETRI_BLACKBOX_REPEAT  run the set this many times in fresh processes,
#                          each under a different test schedule (nightly); default 1
#   PETRI_REQUIRE_*        turn a skipped asset into a failure (see README.md)
set -euo pipefail
cd "$(dirname "$0")/.."

repeat=${PETRI_BLACKBOX_REPEAT:-1}
run_id=${PETRI_EVIDENCE_RUN:-$(date -u +%Y%m%dT%H%M%SZ)-$$}
if [ -z "${PETRI_EVIDENCE_DIR:-}" ]; then
  export PETRI_EVIDENCE_DIR="$PWD/target/fabro-evidence/$run_id"
  mkdir -p "$PETRI_EVIDENCE_DIR"
  ln -sfn "$PETRI_EVIDENCE_DIR" target/fabro-evidence/latest
fi
mkdir -p "$PETRI_EVIDENCE_DIR"
matrix=${PETRI_SCENARIO_MATRIX:-crates/fabro/acceptance/scenarios/matrix.json}
export PETRI_FABRO_COVERAGE_DIR="${PETRI_FABRO_COVERAGE_DIR:-$PETRI_EVIDENCE_DIR/cells}"

# The routing oracle cell of the matrix is satisfied by the acceptance
# crate's oracle test, which writes its own cell record, so it runs here too.
filter='(package(petri-cli) & (binary(/^fabro_.*blackbox$/) | binary(standalone) | binary(fabro_cli))) | (package(petri-fabro-acceptance) & test(every_case_matches_the_fabro_oracle))'
# Schedules for repeated runs: the default parallelism, one test at a time,
# then a narrow pool. Different interleavings shake out shared-resource races.
schedules=("" "--test-threads=1" "--test-threads=2")
status=0
for ((i = 1; i <= repeat; i++)); do
  args=()
  if [ "$repeat" -gt 1 ]; then
    schedule=${schedules[$(((i - 1) % ${#schedules[@]}))]}
    [ -n "$schedule" ] && args+=("$schedule")
    args+=(--no-fail-fast)
    echo "=== black box run $i of $repeat ${schedule:-(default schedule)}"
    export PETRI_EVIDENCE_REPEAT="$i"
  fi
  started=$SECONDS
  # The same build as `mise run test` (workspace, all targets, all features),
  # narrowed by the filter, so the two share one build cache.
  if ! cargo nextest run --locked --workspace --all-targets --all-features --profile ci \
      -E "$filter" --no-tests=fail ${args[@]+"${args[@]}"} "$@"; then
    status=1
  fi
  echo "black box run took $((SECONDS - started)) s"
  if [ -f target/nextest/ci/junit.xml ]; then
    cp target/nextest/ci/junit.xml "$PETRI_EVIDENCE_DIR/junit${PETRI_EVIDENCE_REPEAT:+-$PETRI_EVIDENCE_REPEAT}.xml"
  fi
done

report=(python3 scripts/fabro-coverage-report.py --evidence "$PETRI_EVIDENCE_DIR" --strict)
[ -f "$matrix" ] && report+=(--matrix "$matrix")
[ -f "$PETRI_EVIDENCE_DIR/junit.xml" ] && report+=(--junit "$PETRI_EVIDENCE_DIR/junit.xml")
"${report[@]}" || status=1
echo "evidence: $PETRI_EVIDENCE_DIR"
exit "$status"
