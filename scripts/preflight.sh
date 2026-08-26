#!/usr/bin/env bash
# Run exactly what CI runs, in CI's order, with no cached-silence. Anything that
# passes here and fails there is a workflow bug, not a code bug.
set -e
echo "== fmt"
cargo fmt --all --check
echo "== clippy (flags forced, so the cache cannot stay quiet)"
cargo clippy --all-targets --all-features -- -D warnings
echo "== tests"
cargo test --workspace --all-features 2>&1 | grep -E 'test result' | awk '{p+=$4; f+=$6} END {print p" passed, "f" failed"; if (f>0) exit 1}'
echo "== release"
cargo build --workspace --release
echo "ALL GATES PASS"
