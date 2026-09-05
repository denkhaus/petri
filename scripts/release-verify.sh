#!/usr/bin/env bash
# Verify the delivered files, normal pinned-plugin execution, and tamper refusal.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo 'Usage: scripts/release-verify.sh <archive.tar.gz>' >&2
  exit 2
fi

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT HUP INT TERM
tar -xzf "$1" -C "$work"
for kind in HOST DOCKER DAYTONA; do
  unset "PETRI_SANDBOX_${kind}_PLUGIN" "PETRI_SANDBOX_${kind}_SHA256"
done
for kind in host docker daytona; do
  (
    cd "$work"
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum --check "sandbox-driver-$kind.sha256"
    else
      shasum -a 256 --check "sandbox-driver-$kind.sha256"
    fi
  )
done
unset PETRI_SANDBOX_PLUGIN_DEV
"$work/petri" --version

verify_plugin() {
  local kind=$1
  local workflow=$2
  local success="$work/$kind-success.log"
  local tampered="$work/$kind-tampered.log"
  if ! "$work/petri" run --format native --run-dir "$work/$kind-run" "$workflow" > "$success" 2>&1; then
    cat "$success" >&2
    exit 1
  fi
  grep -q 'release-plugin-ok' "$success"
  # Preserve the executable shape so a loader error cannot mask verification.
  cp "$work/sandbox-driver-$kind" "$work/$kind.original"
  printf '\n' >> "$work/sandbox-driver-$kind"
  if "$work/petri" run --format native --run-dir "$work/$kind-tampered" "$workflow" > "$tampered" 2>&1; then
    echo "The release accepted a modified $kind plugin." >&2
    exit 1
  fi
  if ! grep -qi 'sha256\|checksum' "$tampered"; then
    cat "$tampered" >&2
    echo "The modified $kind plugin failed for a reason other than checksum verification." >&2
    exit 1
  fi
  mv "$work/$kind.original" "$work/sandbox-driver-$kind"
}

cat > "$work/host.yml" <<'WORKFLOW'
scopes:
  main:
    runtime: host
nodes:
  smoke:
    scope: main
    shell: sh
    run: echo release-plugin-ok
WORKFLOW
verify_plugin host "$work/host.yml"

if ! docker info >/dev/null 2>&1; then
  if [[ ${PETRI_REQUIRE_DOCKER:-} == 1 ]]; then
    echo 'Docker is required for the release execution checks.' >&2
    exit 1
  fi
  echo 'Archive and Host execution checks passed; Docker execution checks skipped (daemon unavailable).'
  exit 0
fi

cat > "$work/smoke.yml" <<'WORKFLOW'
scopes:
  main:
    runtime: { container: { image: alpine:3.20 } }
nodes:
  smoke:
    scope: main
    shell: sh
    run: echo release-plugin-ok
WORKFLOW

verify_plugin docker "$work/smoke.yml"
echo 'Release archive, pinned Host and Docker execution, and tamper checks passed.'
