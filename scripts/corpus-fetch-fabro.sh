#!/usr/bin/env bash
# Fetch the Fabro compatibility corpus: the Fabro repository at the pinned
# commit, depth 1, into crates/fabro/corpus/fabro. The frontend's acceptance
# battery lowers every `.fabro` and `.dot` workflow it finds there, and the
# oracle parity harness builds the pinned `fabro` binary from it.
#
#   scripts/corpus-fetch-fabro.sh            fetch at the pin
#   scripts/corpus-fetch-fabro.sh --repin    re-resolve HEAD and rewrite the pin
#
# The pin file holds the commit and, optionally, a `# ref:` line naming the
# ref that commit is reachable through (for example `refs/pull/844/head` for a
# commit that is not on `main`). GitHub serves a depth-1 fetch of any commit
# that some ref reaches; when it refuses the bare SHA, the ref is fetched and
# checked to resolve to the pinned SHA. A ref that resolves elsewhere fails.
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
ref=""
if [ "$REPIN" -eq 0 ] && [ -f "$PIN_FILE" ]; then
  sha=$(awk '$1 !~ /^#/ && NF { print $1; exit }' "$PIN_FILE")
  ref=$(awk '/^# ref:/ { print $3; exit }' "$PIN_FILE")
fi
if [ -z "$sha" ]; then
  sha=$(git ls-remote "$SOURCE" HEAD | awk '{ print $1 }')
  [ -n "$sha" ] || { echo "error: could not resolve HEAD of $SOURCE" >&2; exit 1; }
fi

fetch_pinned() {
  # By SHA first: GitHub allows it for any reachable commit.
  if git -C "$DIR" fetch -q --depth 1 "$SOURCE" "$sha" 2>/dev/null; then
    return 0
  fi
  [ -n "$ref" ] || return 1
  echo "fetch by SHA refused; fetching $ref and checking it is $sha" >&2
  git -C "$DIR" fetch -q --depth 1 "$SOURCE" "$ref" || return 1
  local got
  got=$(git -C "$DIR" rev-parse FETCH_HEAD)
  if [ "$got" != "$sha" ]; then
    echo "error: $ref resolves to $got, not the pinned $sha; the pin and the ref disagree" >&2
    return 1
  fi
}

mkdir -p "$DIR"
if [ "$(git -C "$DIR" rev-parse HEAD 2>/dev/null)" = "$sha" ]; then
  echo "fabro sources already at $sha"
else
  git -C "$DIR" init -q 2>/dev/null || true
  if fetch_pinned && git -C "$DIR" checkout -qf FETCH_HEAD; then
    echo "fabro sources fetched at $sha (depth 1)"
  else
    echo "error: depth-1 fetch of $SOURCE at $sha failed" >&2
    exit 1
  fi
fi
[ "$(git -C "$DIR" rev-parse HEAD)" = "$sha" ] || {
  echo "error: checkout is not at $sha" >&2
  exit 1
}

count=$( { find "$DIR/.fabro" "$DIR/docs" -name '*.fabro' 2>/dev/null || true; find "$DIR/test/attractor" -name '*.dot' 2>/dev/null || true; } | wc -l | tr -d ' ')
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
    echo '# A `# ref: <refname>` line names the ref the commit is reachable through when it is not on main.'
    echo "$sha"
  } > "$PIN_FILE"
  echo "repinned $PIN_FILE"
fi
echo "$count workflows"
