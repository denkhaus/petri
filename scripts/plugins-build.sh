#!/usr/bin/env bash
# Install the sandbox-driver plugin executables the tests launch, from the
# sandbox-driver revision Petri's workspace manifest pins, under
# `target/plugins/bin`. With a release target, install all three plugins
# under `target/<target>/plugins/bin` for bundling. Cargo skips a plugin
# already installed from the same revision.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
if [[ $# -gt 1 ]]; then
  echo "Usage: scripts/plugins-build.sh [release-target]" >&2
  exit 2
fi
install_root="$root/target/plugins"
packages=(sandbox-driver-docker-plugin)
target_args=()
if [[ $# -eq 1 ]]; then
  install_root="$root/target/$1/plugins"
  target_args=(--target "$1")
  packages+=(sandbox-driver-host-plugin sandbox-driver-daytona-plugin)
fi
manifest="$root/Cargo.toml"
url="$(grep -E '^sandbox-driver = \{ git = "' "$manifest" | sed -E 's/.*git = "([^"]+)".*/\1/')"
rev="$(grep -E '^sandbox-driver = \{ git = "' "$manifest" | sed -E 's/.*rev = "([^"]+)".*/\1/')"
if [ -z "$url" ] || [ -z "$rev" ]; then
  echo "plugins-build: the sandbox-driver pin was not found in $manifest" >&2
  exit 1
fi

cargo install --quiet --locked \
  --git "$url" --rev "$rev" \
  --root "$install_root" \
  ${target_args[@]+"${target_args[@]}"} \
  "${packages[@]}"
echo "plugins: $install_root/bin ($rev)"
