#!/usr/bin/env bash
# Install the sandbox-driver plugin executables the tests launch, from the
# sandbox-driver revision Petri's workspace manifest pins, under
# `target/plugins/bin`. With a release target, install all three plugins
# under `target/<target>/plugins/bin` for bundling. Cargo skips a plugin
# already installed from the same revision. Set SANDBOX_DRIVER_SOURCE to a
# local checkout to build the provider packages during coordinated development.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
if [[ $# -gt 1 ]]; then
  echo "Usage: scripts/plugins-build.sh [release-target]" >&2
  exit 2
fi
install_root="$root/target/plugins"
kinds=(host docker)
target_args=()
if [[ $# -eq 1 ]]; then
  install_root="$root/target/$1/plugins"
  target_args=(--target "$1")
  kinds+=(daytona)
fi
if [[ -n "${SANDBOX_DRIVER_SOURCE:-}" ]]; then
  source_root=$(cd "$SANDBOX_DRIVER_SOURCE" && pwd)
  for kind in "${kinds[@]}"; do
    # The executable names are unchanged from the former wrapper packages.
    cargo install --quiet --locked --force \
      --path "$source_root/crates/sandbox-driver-$kind" \
      --root "$install_root" \
      ${target_args[@]+"${target_args[@]}"}
  done
  echo "plugins: $install_root/bin (local checkout)"
  exit 0
fi

packages=()
for kind in "${kinds[@]}"; do
  packages+=("sandbox-driver-$kind")
done
manifest="$root/Cargo.toml"
url="$(grep -E '^sandbox-driver = \{ git = "' "$manifest" | sed -E 's/.*git = "([^"]+)".*/\1/')"
rev="$(grep -E '^sandbox-driver = \{ git = "' "$manifest" | sed -E 's/.*rev = "([^"]+)".*/\1/')"
if [ -z "$url" ] || [ -z "$rev" ]; then
  echo "plugins-build: the sandbox-driver pin was not found in $manifest" >&2
  exit 1
fi

# Replace binaries installed by the old wrapper packages once. Later runs
# retain Cargo's normal skip when this exact revision is already installed.
install_args=()
installed=$(cargo install --list --root "$install_root")
for kind in "${kinds[@]}"; do
  if [[ "$installed" == *"sandbox-driver-$kind-plugin "* ]]; then
    install_args=(--force)
    break
  fi
done
cargo install --quiet --locked \
  ${install_args[@]+"${install_args[@]}"} \
  --git "$url" --rev "$rev" \
  --root "$install_root" \
  ${target_args[@]+"${target_args[@]}"} \
  "${packages[@]}"
echo "plugins: $install_root/bin ($rev)"
