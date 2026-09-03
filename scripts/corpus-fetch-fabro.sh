#!/usr/bin/env bash
# Fetch the Fabro compatibility corpus: the Fabro repository at the pinned
# commit, depth 1, into crates/fabro/corpus/fabro. The frontend's acceptance
# battery lowers every `.fabro` and `.dot` workflow it finds there.
#
#   scripts/corpus-fetch-fabro.sh            fetch at the pin
#   scripts/corpus-fetch-fabro.sh --repin    re-resolve HEAD and rewrite the pin
#
# FABRO_SOURCE overrides the fetch URL (a local checkout works), for offline use.
set -euo pipefail
cd "$(dirname "$0")/.."

PIN_FILE=crates/fabro/corpus-pin.txt
REPO=fabro-sh/fabro
SOURCE="${FABRO_SOURCE:-https://github.com/$REPO}"
DIR=crates/fabro/corpus/fabro
REPIN=0
for arg in "$@"; do
  case "$arg" in
    --repin) REPIN=1 ;;
    *) echo "usage: $0 [--repin]" >&2; exit 2 ;;
  esac
done

sha=""
if [ "$REPIN" -eq 0 ] && [ -f "$PIN_FILE" ]; then
  sha=$(awk '$1 !~ /^#/ && NF { print $1; exit }' "$PIN_FILE")
fi
if [ -z "$sha" ]; then
  sha=$(git ls-remote "$SOURCE" HEAD | awk '{ print $1 }')
  [ -n "$sha" ] || { echo "error: could not resolve HEAD of $SOURCE" >&2; exit 1; }
fi

mkdir -p "$DIR"
if [ "$(git -C "$DIR" rev-parse HEAD 2>/dev/null)" = "$sha" ]; then
  echo "fabro sources already at $sha"
else
  git -C "$DIR" init -q 2>/dev/null || true
  if git -C "$DIR" fetch -q --depth 1 "$SOURCE" "$sha" && git -C "$DIR" checkout -qf FETCH_HEAD; then
    echo "fabro sources fetched at $sha (depth 1)"
  else
    echo "error: depth-1 fetch of $SOURCE at $sha failed" >&2
    exit 1
  fi
fi

count=$( (find "$DIR/.fabro" "$DIR/docs" -name '*.fabro' 2>/dev/null; find "$DIR/test/attractor" -name '*.dot' 2>/dev/null) | wc -l | tr -d ' ')
cat > "$DIR/PROVENANCE.md" <<PROV
# $REPO

- Source: https://github.com/$REPO
- Commit: $sha
- Contents: full source tree at the pin
- Fetched: $(date -u +%Y-%m-%dT%H:%MZ)
- Files: $count workflow(s) under .fabro/, docs/ and test/attractor/

Fetched for compatibility testing of the Fabro frontend. The files are the
property of their authors and remain under the repository's licence.
PROV

if [ "$REPIN" -eq 1 ]; then
  {
    echo '# The Fabro commit the compatibility corpus and the oracle fixtures are generated from.'
    echo '# scripts/corpus-fetch-fabro.sh fetches this commit; `--repin` re-resolves HEAD and rewrites it.'
    echo "$sha"
  } > "$PIN_FILE"
  echo "repinned $PIN_FILE"
fi
echo "$count workflows"
