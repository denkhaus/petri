#!/usr/bin/env bash
# Run the pinned Fabro comparison matrix: the shipped `petri` binary and the
# pinned `fabro` binary on the same scenarios, compared under the rules in
# crates/fabro/acceptance/CONTRACT.md. Evidence records go to the same
# run-scoped directory as the black box run (PETRI_EVIDENCE_DIR).
#
#   scripts/test-fabro-differential.sh [nextest args...]
#
# The Fabro binary comes from scripts/fabro-binary.sh (built from the fetched
# corpus, never from PATH) and is handed to the tests as PETRI_FABRO_BIN.
# Without it the matrix skips with a notice; PETRI_REQUIRE_FABRO_BINARY makes
# that a failure, and CI sets it.
#
# The test target is `petri-cli`'s `fabro_differential` binary
# (crates/petri/cli/tests/fabro_differential.rs), owned by the differential
# task. Until it exists this script fails under PETRI_REQUIRE_FABRO_BINARY and
# skips otherwise, so the CI job can never pass on a missing matrix.
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
manifest=${PETRI_SCENARIO_MANIFEST:-crates/fabro/acceptance/scenarios/manifest.json}

bin=$(scripts/fabro-binary.sh)
if [ -z "$bin" ]; then
  # fabro-binary.sh printed the skip notice, or exited 1 under the require flag.
  echo "skipping: the differential matrix needs the pinned fabro binary"
  exit 0
fi
export PETRI_FABRO_BIN="$bin"
echo "fabro: $bin ($("$bin" --version))"

if [ ! -f "crates/petri/cli/tests/$TEST.rs" ]; then
  if [ -n "${PETRI_REQUIRE_FABRO_BINARY:-}" ]; then
    echo "error: the differential matrix crates/petri/cli/tests/$TEST.rs does not exist; the compatibility job cannot pass without it" >&2
    exit 1
  fi
  echo "skipping: crates/petri/cli/tests/$TEST.rs does not exist yet"
  exit 0
fi

status=0
started=$SECONDS
cargo nextest run --locked -p petri-cli --profile ci --test "$TEST" --no-tests=fail "$@" || status=1
echo "differential run took $((SECONDS - started)) s"
[ -f target/nextest/ci/junit.xml ] && cp target/nextest/ci/junit.xml "$PETRI_EVIDENCE_DIR/junit-differential.xml"

report=(python3 scripts/fabro-coverage-report.py --evidence "$PETRI_EVIDENCE_DIR" --strict)
[ -f "$manifest" ] && report+=(--manifest "$manifest")
"${report[@]}" || status=1
echo "evidence: $PETRI_EVIDENCE_DIR"
exit "$status"
