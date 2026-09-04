#!/usr/bin/env bash
# Install the sandbox-driver plugin executables the tests launch, from the
# sandbox-driver revision Petri's workspace manifest pins, under
# `target/plugins/bin`. `cargo install` skips a plugin already installed
# from the same revision, so this is cheap to run before every test task.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$root/Cargo.toml"
url="$(grep -E '^sandbox-driver = \{ git = "' "$manifest" | sed -E 's/.*git = "([^"]+)".*/\1/')"
rev="$(grep -E '^sandbox-driver = \{ git = "' "$manifest" | sed -E 's/.*rev = "([^"]+)".*/\1/')"
if [ -z "$url" ] || [ -z "$rev" ]; then
  echo "plugins-build: the sandbox-driver pin was not found in $manifest" >&2
  exit 1
fi

cargo install --quiet --locked \
  --git "$url" --rev "$rev" \
  --root "$root/target/plugins" \
  sandbox-driver-docker-plugin
echo "plugins: $root/target/plugins/bin/sandbox-driver-docker ($rev)"
