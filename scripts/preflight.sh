#!/usr/bin/env bash
# Compatibility entry point. Mise owns the verification commands.
set -euo pipefail
exec mise run check
