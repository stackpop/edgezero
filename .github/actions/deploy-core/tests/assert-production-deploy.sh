#!/usr/bin/env bash
set -euo pipefail

# Asserts the production deploy path end to end: build-app-cli -> deploy-fastly ->
# the app-owned CLI -> the manifest's overridden Fastly deploy command.
#
# Also asserts the provider-env credential boundary: the typed inputs reach the
# deploy, inherited aliases are CLEARED, and the action's own secret-bearing
# helper variables do NOT survive into the CLI's environment.
#
# Reads (env):
#   GITHUB_WORKSPACE                      required  checkout root (holds the smoke fixture output)
#   EDGEZERO__TEST__FASTLY_VERSION        required  the production deploy's fastly-version output
#   EDGEZERO__TEST__PREVIOUS_VERSION      required  the captured rollback target (previous-version)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local version_out="${EDGEZERO__TEST__FASTLY_VERSION:-}"
  local previous_out="${EDGEZERO__TEST__PREVIOUS_VERSION:-}"
  local env_seen="$workspace/fixture-app/env-seen.txt"
  local argv="$workspace/fixture-app/deploy-argv.txt"

  [[ -f "$argv" && -f "$env_seen" ]] ||
    fail "the deploy never reached the app CLI's Fastly deploy command"
  echo "recorded argv:"
  cat "$argv"
  echo "environment the deploy saw:"
  cat "$env_seen"

  [[ "$version_out" == "7" ]] ||
    fail "expected fastly-version=7 out of the action, got '${version_out:-<empty>}'"

  # The rollback target was captured BEFORE the deploy via `active-version`: the
  # fake Fastly API reports version 40 active, so previous-version must be 40. A
  # non-zero active-version exit would have failed the deploy closed instead.
  [[ "$previous_out" == "40" ]] ||
    fail "expected previous-version=40 (the captured rollback target), got '${previous_out:-<empty>}'"

  # The action supplies --non-interactive itself, so a manifest-command deploy
  # (this fixture is one) cannot block on a TTY prompt in CI.
  grep -qx -- '--non-interactive' "$argv" ||
    fail "the action-owned --non-interactive never reached the deploy command"

  # The provider-env boundary: typed values in, inherited aliases out, and none
  # of the action's private secret carriers left behind.
  local expected
  for expected in \
    'token=dummy-token' \
    'service-id=dummy-service' \
    'endpoint=CLEARED' \
    'home=CLEARED' \
    'action-token-carrier=CLEARED' \
    'provider-env-json=CLEARED'; do
    grep -qx -- "$expected" "$env_seen" ||
      fail "credential boundary violated: expected '$expected' in env-seen.txt"
  done

  notice "production deploy, version threading, and the credential boundary all hold"
}

main "$@"
