#!/usr/bin/env bash
set -euo pipefail

# Each GitHub deploy key belongs to one repository. SSH aliases ensure Git
# offers the correct key, even when several keys authenticate to github.com.
: "${SANDBOX_DRIVER_DEPLOY_KEY:?SANDBOX_DRIVER_DEPLOY_KEY is required}"
: "${PEBBLE_DEPLOY_KEY:?PEBBLE_DEPLOY_KEY is required}"
: "${LITHOS_LLM_DEPLOY_KEY:?LITHOS_LLM_DEPLOY_KEY is required}"
: "${RUNNER_TEMP:?RUNNER_TEMP is required}"
: "${ACTION_PATH:?ACTION_PATH is required}"
umask 077
credentials="$RUNNER_TEMP/petri-dependencies"
mkdir -p "$credentials"
printf '%s\n' "$SANDBOX_DRIVER_DEPLOY_KEY" > "$credentials/sandbox-driver"
printf '%s\n' "$PEBBLE_DEPLOY_KEY" > "$credentials/pebble"
printf '%s\n' "$LITHOS_LLM_DEPLOY_KEY" > "$credentials/lithos-llm"
: > "$credentials/ssh_config"
configure() {
  local owner="$1" repository="$2"
  cat >> "$credentials/ssh_config" <<CONFIG
Host petri-$repository
  HostName github.com
  HostKeyAlias github.com
  User git
  IdentityFile "$credentials/$repository"
  IdentitiesOnly yes
  IdentityAgent none
  StrictHostKeyChecking yes
  UserKnownHostsFile "$ACTION_PATH/known_hosts"
CONFIG
  git config --global "url.ssh://git@petri-$repository/$owner/$repository.insteadOf" \
    "ssh://git@github.com/$owner/$repository"
}
for repository in sandbox-driver pebble lithos-llm; do
  configure lithoscomputer "$repository"
done
# The Fabro black box bundle sources (scripts/corpus-fetch-fabro-bundles.sh).
# Optional: a job that does not fetch bundles leaves these keys empty.
if [ -n "${CODE_REVIEW_DEPLOY_KEY:-}" ]; then
  printf '%s\n' "$CODE_REVIEW_DEPLOY_KEY" > "$credentials/code-review"
  configure lithoscomputer code-review
fi
if [ -n "${FACTORY_DEPLOY_KEY:-}" ]; then
  printf '%s\n' "$FACTORY_DEPLOY_KEY" > "$credentials/factory"
  configure veniceai factory
fi
printf -v ssh_command 'ssh -F %q' "$credentials/ssh_config"
git config --global core.sshCommand "$ssh_command"
