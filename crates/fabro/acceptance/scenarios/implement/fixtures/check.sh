#!/bin/sh
# The fixture's own deterministic check: the state file must say `good`.
if grep -qx good state.txt 2>/dev/null; then
  echo CHECK_PASSED
  exit 0
fi
echo "CHECK_FAILED: state.txt is $(cat state.txt 2>/dev/null || echo missing)"
exit 1
