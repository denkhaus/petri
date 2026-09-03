#!/usr/bin/env bash
# Regenerate the Fabro oracle fixtures in crates/fabro/oracle/expected/ from the
# shared cases in crates/fabro/oracle/cases/, by running the cases through the
# pinned Fabro checkout (scripts/corpus-fetch-fabro.sh fetches it). Then record
# Petri's own expected results for the cases that declare a deliberate
# departure. Run this when the Fabro pin or a case changes; Petri CI only
# compares against the committed fixtures.
set -euo pipefail
cd "$(dirname "$0")/.."

CORPUS=crates/fabro/corpus/fabro
[ -d "$CORPUS/lib" ] || { echo "error: the Fabro corpus is not fetched; run scripts/corpus-fetch-fabro.sh" >&2; exit 1; }
commit=$(git -C "$CORPUS" rev-parse HEAD)
GEN=crates/fabro/oracle/generator

# Fabro's own lock file pins the generator's dependency versions.
cp "$CORPUS/Cargo.lock" "$GEN/Cargo.lock"
export CARGO_TARGET_DIR="${FABRO_ORACLE_TARGET_DIR:-$GEN/target}"
(cd "$GEN" && cargo run --quiet --release -- ../cases ../expected "$commit")

PETRI_ORACLE_RECORD=1 cargo nextest run -p petri-fabro-acceptance --test routing every_case_matches_the_fabro_oracle
echo "regenerated crates/fabro/oracle/expected at fabro $commit"
