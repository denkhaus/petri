#!/usr/bin/env bash
# Second pass: vendor every local action a corpus workflow references (`uses: ./…`),
# at each repo's pinned commit, so composite inlining has the files it needs.
set -uo pipefail
cd "$(dirname "$0")/.."
for repodir in corpus/*/; do
  repo=$(basename "$repodir" | sed 's/__/\//')
  sha=$(grep -oE 'Commit: [0-9a-f]+' "$repodir/PROVENANCE.md" | awk '{print $2}')
  [ -n "$sha" ] || continue
  refs=$(grep -rhoE 'uses:\s*\./[^ #"'"'"']*' "$repodir/.github/workflows" 2>/dev/null | sed -E 's/uses:\s*\.\///' | sort -u)
  for ref in $refs; do
    ref="${ref%/}"
    [ -z "$ref" ] && continue
    for candidate in action.yml action.yaml; do
      target="$repodir/$ref/$candidate"
      [ -f "$target" ] && break
      if curl -sfL "https://raw.githubusercontent.com/$repo/$sha/$ref/$candidate" -o "$target" --create-dirs; then
        echo "  $repo: $ref/$candidate"
        break
      fi
    done
  done
done
echo "done"
