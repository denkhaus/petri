#!/usr/bin/env bash
# Resolve the pinned Fabro binary that the parity and differential harnesses
# launch, and print its path on stdout.
#
#   scripts/fabro-binary.sh             print the path; build the binary when it is missing
#   scripts/fabro-binary.sh --no-build  print the path only when the binary already exists
#
# The binary is built from the fetched corpus (scripts/corpus-fetch-fabro.sh)
# into crates/fabro/corpus/fabro-target/ (gitignored; FABRO_ORACLE_TARGET_DIR
# overrides the directory). It must report the pinned short SHA. FABRO_BIN
# overrides the path and is checked the same way. A `fabro` on PATH is never
# used. This is testing and parity tooling: nothing links a Fabro crate.
#
# Exit status:
#   0  the path is on stdout
#   0  with an empty stdout and a "skipping" notice on stderr when the corpus
#      is not fetched or the binary is absent under --no-build, unless
#      PETRI_REQUIRE_FABRO_BINARY is set
#   1  when PETRI_REQUIRE_FABRO_BINARY is set and the binary is unavailable,
#      when a build fails, or when the binary reports another commit
set -euo pipefail
cd "$(dirname "$0")/.."

CORPUS=crates/fabro/corpus/fabro
PIN_FILE=crates/fabro/corpus-pin.txt
TARGET="${FABRO_ORACLE_TARGET_DIR:-$PWD/crates/fabro/corpus/fabro-target}"
BUILD=1
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    *) echo "usage: $0 [--no-build]" >&2; exit 2 ;;
  esac
done

required() { [ -n "${PETRI_REQUIRE_FABRO_BINARY:-}" ]; }
unavailable() {
  if required; then
    echo "error: PETRI_REQUIRE_FABRO_BINARY is set, but $1" >&2
    exit 1
  fi
  echo "skipping: $1" >&2
  exit 0
}

pin=$(awk '$1 !~ /^#/ && NF { print $1; exit }' "$PIN_FILE")
[ -n "$pin" ] || { echo "error: no pin in $PIN_FILE" >&2; exit 1; }

if [ -n "${FABRO_BIN:-}" ]; then
  bin="$FABRO_BIN"
  [ -x "$bin" ] || unavailable "FABRO_BIN=$bin is not an executable"
else
  bin="$TARGET/debug/fabro"
  if [ ! -x "$bin" ]; then
    [ "$BUILD" -eq 1 ] || unavailable "the pinned fabro binary is not built at $bin (run scripts/fabro-binary.sh without --no-build)"
    [ -d "$CORPUS/lib" ] || unavailable "the Fabro corpus is not fetched; run scripts/corpus-fetch-fabro.sh"
    commit=$(git -C "$CORPUS" rev-parse HEAD)
    [ "$commit" = "$pin" ] || {
      echo "error: the corpus is at $commit but the pin is $pin; run scripts/corpus-fetch-fabro.sh" >&2
      exit 1
    }
    echo "building fabro-cli from $CORPUS at $pin into $TARGET" >&2
    started=$SECONDS
    (cd "$CORPUS" && CARGO_TARGET_DIR="$TARGET" cargo build --locked -q -p fabro-cli) || {
      echo "error: the pinned fabro build failed" >&2
      exit 1
    }
    echo "built $bin in $((SECONDS - started)) s" >&2
  fi
fi

version=$("$bin" --version 2>/dev/null || true)
case "$version" in
  *"${pin:0:7}"*) ;;
  *)
    echo "error: $bin reports '$version', not the pinned commit $pin" >&2
    exit 1
    ;;
esac
echo "$bin"
