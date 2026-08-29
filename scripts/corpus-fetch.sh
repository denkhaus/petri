#!/usr/bin/env bash
# Fetch well-known OSS repositories at a pinned commit for the compatibility corpus.
# Each repo gets crates/github/corpus/<owner>__<repo>/ with its full source tree at
# the pin (a depth-1 fetch of the commit, `.git` included) — the run sweep then
# exercises real version-file reads, local actions, and git-dependent actions.
# Always the whole tree: a workflows-only corpus made checkout lie, and workflows
# that read any repo file (version files, hashFiles patterns, local scripts) failed
# for the corpus's sin, not petri's. A failed fetch is an error, never a silent
# fallback. A PROVENANCE.md records the commit, the licence, and when.
#
# The corpus data is not committed — crates/github/corpus/ is gitignored apart from
# REPORT.md. Commits come from crates/github/corpus-pins.txt, which is committed, so
# two fetches of the same pins produce the same corpus. `--repin` re-resolves HEAD for
# every repo and rewrites the pins file.
set -euo pipefail
cd "$(dirname "$0")/.."

PINS=crates/github/corpus-pins.txt
REPIN=0
for arg in "$@"; do
  case "$arg" in
    --repin) REPIN=1 ;;
    *) echo "usage: $0 [--repin]" >&2; exit 2 ;;
  esac
done

REPOS=(
  rust-lang/cargo
  tokio-rs/tokio
  serde-rs/serde
  BurntSushi/ripgrep
  sharkdp/bat
  astral-sh/ruff
  astral-sh/uv
  pola-rs/polars
  cli/cli
  denoland/deno
  facebook/react
  vercel/next.js
  tailwindlabs/tailwindcss
  prometheus/prometheus
  hashicorp/terraform
  rails/rails
  django/django
  python/cpython
  nodejs/node
  ohmyzsh/ohmyzsh
  actions/checkout
  golang/tools
)

# The sha recorded for $1 in the pins file, or empty when it has none.
pinned_sha() {
  [ -f "$PINS" ] || return 0
  awk -v repo="$1" '$1 == repo { print $2; exit }' "$PINS"
}

declare -a RESOLVED=()

for repo in "${REPOS[@]}"; do
  owner="${repo%%/*}"; name="${repo##*/}"
  dir="crates/github/corpus/${owner}__${name}"
  mkdir -p "$dir/.github/workflows"
  echo "== $repo"
  sha=""
  if [ "$REPIN" -eq 0 ]; then
    sha=$(pinned_sha "$repo")
  fi
  if [ -n "$sha" ]; then
    echo "   pinned at $sha"
  else
    sha=$(gh api "repos/$repo/commits/HEAD" --jq .sha 2>/dev/null || echo unknown)
  fi
  RESOLVED+=("$repo $sha")
  license=$(gh api "repos/$repo" --jq '.license.spdx_id // "unknown"' 2>/dev/null || echo unknown)
  default_branch=$(gh api "repos/$repo" --jq .default_branch 2>/dev/null || echo main)

  if [ "$sha" = "unknown" ]; then
    echo "error: could not resolve a commit for $repo" >&2
    exit 1
  fi

  # The full source tree at the pin: one depth-1 fetch of the commit, checked
  # out detached, `.git` included.
  contents="full source tree at the pin"
  if [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null)" = "$sha" ]; then
    echo "   sources already at $sha"
  else
    git -C "$dir" init -q 2>/dev/null || true
    if git -C "$dir" fetch -q --depth 1 "https://github.com/$repo" "$sha"       && git -C "$dir" checkout -qf FETCH_HEAD; then
      echo "   sources fetched at $sha (depth 1)"
    else
      echo "error: depth-1 fetch of $repo at $sha failed" >&2
      exit 1
    fi
  fi
  count=$(ls "$dir/.github/workflows" 2>/dev/null | grep -cE '\.(yml|yaml)$' || true)
  cat > "$dir/PROVENANCE.md" <<EOF
# $repo

- Source: https://github.com/$repo
- Commit: $sha (default branch: $default_branch)
- Licence: $license
- Contents: $contents
- Fetched: $(date -u +%Y-%m-%dT%H:%MZ)
- Files: $count workflow(s) under .github/workflows

Fetched for compatibility testing of the GitHub Actions frontend. The files are
the property of their authors and remain under the licence above.
EOF
  echo "   $count workflows, licence $license, $contents"
done

if [ "$REPIN" -eq 1 ]; then
  {
    echo '# Pinned commits for the compatibility corpus, one `owner/repo <sha>` per line.'
    echo '# scripts/corpus-fetch.sh fetches each repo at the sha recorded here, so a fetch is'
    echo '# reproducible; `scripts/corpus-fetch.sh --repin` re-resolves HEAD and rewrites this file.'
    printf '%s\n' "${RESOLVED[@]}" | sort
  } > "$PINS"
  echo "repinned $PINS"
fi

echo "done"
