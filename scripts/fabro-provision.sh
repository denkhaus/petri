#!/usr/bin/env bash
# Provision the pinned Fabro binary for Petri's parity and differential
# harnesses, and print its path.
#
#   scripts/fabro-provision.sh            build (or reuse) and print the path
#   scripts/fabro-provision.sh --check    print the path only when a valid
#                                         binary already exists; exit 3 when
#                                         it does not
#
# This is testing and parity tooling. The binary is built with
# `cargo build --locked -p fabro-cli` from the fetched corpus checkout
# (scripts/corpus-fetch-fabro.sh) into crates/fabro/corpus/fabro-target/, a
# gitignored cache. The corpus must be at the pin in crates/fabro/corpus-pin.txt
# and the built binary must report the pin's short SHA in its version string,
# or the script fails. A `fabro` on PATH is never used. FABRO_BIN overrides the
# binary; it is checked the same way. FABRO_ORACLE_TARGET_DIR overrides the
# cache directory.
#
# Nothing here links a Fabro crate into Petri; the corpus has its own
# Cargo workspace and target directory.
set -euo pipefail
cd "$(dirname "$0")/.."

CORPUS=crates/fabro/corpus/fabro
PIN_FILE=crates/fabro/corpus-pin.txt
TARGET="${FABRO_ORACLE_TARGET_DIR:-$PWD/crates/fabro/corpus/fabro-target}"
CHECK_ONLY=0
case "${1:-}" in
  "") ;;
  --check) CHECK_ONLY=1 ;;
  *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

pin=$(awk '$1 !~ /^#/ && NF { print $1; exit }' "$PIN_FILE")
[ -n "$pin" ] || { echo "error: no pin in $PIN_FILE" >&2; exit 1; }

verify() {
  local bin=$1 version
  version=$("$bin" --version 2>/dev/null) || { echo "error: $bin does not run" >&2; return 1; }
  case "$version" in
    *"${pin:0:7}"*) ;;
    *) echo "error: $bin reports '$version', not the pinned commit $pin" >&2; return 1 ;;
  esac
}

if [ -n "${FABRO_BIN:-}" ]; then
  verify "$FABRO_BIN"
  echo "$FABRO_BIN"
  exit 0
fi

BIN="$TARGET/debug/fabro"
if [ -x "$BIN" ] && verify "$BIN" 2>/dev/null; then
  echo "$BIN"
  exit 0
fi
if [ "$CHECK_ONLY" -eq 1 ]; then
  echo "error: no pinned fabro binary at $BIN; run $0 to build it" >&2
  exit 3
fi

[ -d "$CORPUS/lib" ] || { echo "error: the Fabro corpus is not fetched; run scripts/corpus-fetch-fabro.sh" >&2; exit 1; }
commit=$(git -C "$CORPUS" rev-parse HEAD)
[ "$commit" = "$pin" ] || { echo "error: the corpus is at $commit but the pin is $pin; run scripts/corpus-fetch-fabro.sh" >&2; exit 1; }
echo "building fabro-cli from $CORPUS at $commit into $TARGET" >&2
(cd "$CORPUS" && CARGO_TARGET_DIR="$TARGET" cargo build --locked -q -p fabro-cli)
verify "$BIN"
echo "$BIN"
