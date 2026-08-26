#!/usr/bin/env bash
# Vendor the `.github/workflows` of well-known OSS repositories at a pinned commit.
# Each repo gets crates/github/corpus/<owner>__<repo>/ with its workflows and a PROVENANCE.md
# recording the commit, the licence, and when it was fetched.
set -euo pipefail
cd "$(dirname "$0")/.."

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

for repo in "${REPOS[@]}"; do
  owner="${repo%%/*}"; name="${repo##*/}"
  dir="crates/github/corpus/${owner}__${name}"
  mkdir -p "$dir/.github/workflows"
  echo "== $repo"
  sha=$(gh api "repos/$repo/commits/HEAD" --jq .sha 2>/dev/null || echo unknown)
  license=$(gh api "repos/$repo" --jq '.license.spdx_id // "unknown"' 2>/dev/null || echo unknown)
  default_branch=$(gh api "repos/$repo" --jq .default_branch 2>/dev/null || echo main)
  # List workflow files at that commit.
  files=$(gh api "repos/$repo/contents/.github/workflows?ref=$sha" --jq '.[] | select(.type=="file") | .name' 2>/dev/null || true)
  count=0
  for f in $files; do
    case "$f" in
      *.yml|*.yaml) ;;
      *) continue ;;
    esac
    if curl -sfL "https://raw.githubusercontent.com/$repo/$sha/.github/workflows/$f" -o "$dir/.github/workflows/$f"; then
      count=$((count+1))
    fi
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
  cat > "$dir/PROVENANCE.md" <<EOF
# $repo

- Source: https://github.com/$repo
- Commit: $sha (default branch: $default_branch)
- Licence: $license
- Fetched: $(date -u +%Y-%m-%dT%H:%MZ)
- Files: $count workflow(s) under .github/workflows

Vendored for compatibility testing of the GitHub Actions frontend. The workflow
files are the property of their authors and remain under the licence above.
EOF
  echo "   $count workflows, licence $license"
done
echo "done"
