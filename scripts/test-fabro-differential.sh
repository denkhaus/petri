#!/usr/bin/env bash
# Run the pinned Fabro comparison matrix: the shipped `petri` binary and the
# pinned `fabro` binary on the same scenarios, compared under the rules in
# crates/fabro/acceptance/CONTRACT.md and the decision records under
# crates/fabro/acceptance/decisions/. Evidence records go to the same
# run-scoped directory as the black box run (PETRI_EVIDENCE_DIR).
#
#   scripts/test-fabro-differential.sh [nextest args...]
#
# The Fabro binary comes from scripts/fabro-provision.sh (built from the
# fetched corpus, never from PATH) and is handed to the tests as FABRO_BIN.
# Without it the matrix compares Petri against the committed reference and
# says so; PETRI_REQUIRE_FABRO_BINARY=1 makes a missing binary a failure, and
# CI sets it. The test target is `petri-cli`'s `fabro_differential` binary
# (crates/petri/cli/tests/fabro_differential.rs).
set -euo pipefail
cd "$(dirname "$0")/.."

TEST=fabro_differential
run_id=${PETRI_EVIDENCE_RUN:-$(date -u +%Y%m%dT%H%M%SZ)-$$}
if [ -z "${PETRI_EVIDENCE_DIR:-}" ]; then
  export PETRI_EVIDENCE_DIR="$PWD/target/fabro-evidence/$run_id"
  mkdir -p "$PETRI_EVIDENCE_DIR"
  ln -sfn "$PETRI_EVIDENCE_DIR" target/fabro-evidence/latest
fi
mkdir -p "$PETRI_EVIDENCE_DIR"
matrix=${PETRI_SCENARIO_MATRIX:-crates/fabro/acceptance/scenarios/matrix.json}

if [ -n "${PETRI_REQUIRE_FABRO_BINARY:-}" ]; then
  # Required: build on a miss, fail on any problem.
  FABRO_BIN=$(scripts/fabro-provision.sh)
  export FABRO_BIN
elif bin=$(scripts/fabro-provision.sh --check 2>/dev/null); then
  export FABRO_BIN="$bin"
else
  echo "notice: no pinned fabro binary (scripts/fabro-provision.sh builds it); the matrix compares Petri against the committed reference only"
fi
[ -n "${FABRO_BIN:-}" ] && echo "fabro: $FABRO_BIN ($("$FABRO_BIN" --version))"

if [ ! -f "crates/petri/cli/tests/$TEST.rs" ]; then
  if [ -n "${PETRI_REQUIRE_FABRO_BINARY:-}" ]; then
    echo "error: the differential matrix crates/petri/cli/tests/$TEST.rs does not exist; the compatibility job cannot pass without it" >&2
    exit 1
  fi
  echo "skipping: crates/petri/cli/tests/$TEST.rs does not exist"
  exit 0
fi
export PETRI_FABRO_COVERAGE_DIR="${PETRI_FABRO_COVERAGE_DIR:-$PETRI_EVIDENCE_DIR/cells}"

status=0
started=$SECONDS
# The committed references must come from the pinned Fabro before any of
# them is used as a baseline.
cargo nextest run --locked --workspace --all-targets --all-features --profile ci \
  -E "package(petri-fabro-acceptance) & binary(reference_version)" --no-tests=fail || status=1
cargo nextest run --locked --workspace --all-targets --all-features --profile ci \
  -E "package(petri-cli) & binary($TEST)" --no-tests=fail "$@" || status=1
echo "differential run took $((SECONDS - started)) s"
[ -f target/nextest/ci/junit.xml ] && cp target/nextest/ci/junit.xml "$PETRI_EVIDENCE_DIR/junit-differential.xml"

# Informational here: the matrix's own cells fail through Nextest above; the
# routine gate (scripts/test-fabro-blackbox.sh, CI's check jobs) is strict.
report=(python3 scripts/fabro-coverage-report.py --evidence "$PETRI_EVIDENCE_DIR")
[ -f "$matrix" ] && report+=(--matrix "$matrix")
"${report[@]}" || true
echo "evidence: $PETRI_EVIDENCE_DIR"
exit "$status"
