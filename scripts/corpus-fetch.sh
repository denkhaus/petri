#!/usr/bin/env bash
# Fetch well-known OSS repositories at a pinned commit for the compatibility corpus.
# Each repo gets crates/github/corpus/<owner>__<repo>/ with, preferably, its full
# source tree at the pin (a depth-1 fetch of the commit, `.git` included) — the run
# sweep then exercises real version-file reads, local actions, and git-dependent
# actions. Repositories over PETRI_CORPUS_SOURCE_MAX_MB (default 400, by GitHub's
# reported history size) fall back to workflows-only, since the sweep's checkout
# copies the tree into every job. A PROVENANCE.md records the commit, the licence,
# what was fetched, and when.
#
# The corpus data is not committed — crates/github/corpus/ is gitignored apart from
# REPORT.md. Commits come from crates/github/corpus-pins.txt, which is committed, so
# two fetches of the same pins produce the same corpus. `--repin` re-resolves HEAD for
# every repo and rewrites the pins file.
set -euo pipefail
cd "$(dirname "$0")/.."

PINS=crates/github/corpus-pins.txt
SOURCE_MAX_MB="${PETRI_CORPUS_SOURCE_MAX_MB:-400}"
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
  size_kb=$(gh api "repos/$repo" --jq .size 2>/dev/null || echo 0)

  # The full source tree at the pin, when the repository is small enough: one
  # depth-1 fetch of the commit, checked out detached, `.git` included.
  contents="workflows only"
  if [ "$sha" != "unknown" ] && [ "$size_kb" -le $((SOURCE_MAX_MB * 1024)) ]; then
    if [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null)" = "$sha" ]; then
      contents="full source tree at the pin"
      echo "   sources already at $sha"
    else
      git -C "$dir" init -q 2>/dev/null || true
      if git -C "$dir" fetch -q --depth 1 "https://github.com/$repo" "$sha" 2>/dev/null         && git -C "$dir" checkout -qf FETCH_HEAD 2>/dev/null; then
        contents="full source tree at the pin"
        echo "   sources fetched at $sha (depth 1)"
      else
        echo "   sources fetch failed; falling back to workflows only"
      fi
    fi
  elif [ "$size_kb" -gt $((SOURCE_MAX_MB * 1024)) ]; then
    echo "   sources skipped ($((size_kb / 1024))MB > ${SOURCE_MAX_MB}MB cap)"
  fi

  if [ "$contents" = "workflows only" ]; then
    # List workflow files at that commit.
    files=$(gh api "repos/$repo/contents/.github/workflows?ref=$sha" --jq '.[] | select(.type=="file") | .name' 2>/dev/null || true)
    for f in $files; do
      case "$f" in
        *.yml|*.yaml) ;;
        *) continue ;;
      esac
      curl -sfL "https://raw.githubusercontent.com/$repo/$sha/.github/workflows/$f" -o "$dir/.github/workflows/$f" || true
    done
    # Local composite actions, if any, so `uses: ./.github/actions/x` resolves.
    actions=$(gh api "repos/$repo/contents/.github/actions?ref=$sha" --jq '.[] | select(.type=="dir") | .name' 2>/dev/null || true)
    for a in $actions; do
      for candidate in action.yml action.yaml; do
        if curl -sfL "https://raw.githubusercontent.com/$repo/$sha/.github/actions/$a/$candidate" -o "$dir/.github/actions/$a/$candidate" --create-dirs; then
          break
        fi
      done
    done
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
