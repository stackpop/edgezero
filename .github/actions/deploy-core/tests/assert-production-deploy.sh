#!/usr/bin/env bash
set -euo pipefail

# Asserts the production deploy path end to end: build-cli -> deploy-fastly ->
# the app-owned CLI -> the manifest's overridden Fastly deploy command.
#
# Also asserts the provider-env credential boundary: the typed inputs reach the
# deploy, and an inherited alias (FASTLY_ENDPOINT, set at job level) is CLEARED.
#
# Inputs (environment): GITHUB_WORKSPACE, FASTLY_VERSION_OUT.

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local version_out="${FASTLY_VERSION_OUT:-}"
  local env_seen="$workspace/fixture-app/env-seen.txt"
  local argv="$workspace/fixture-app/deploy-argv.txt"

  if [[ ! -f "$argv" || ! -f "$env_seen" ]]; then
    echo "::error::the deploy never reached the app CLI's Fastly deploy command" >&2
    return 1
  fi
  echo "recorded argv:"; cat "$argv"
  echo "credentials the deploy saw:"; cat "$env_seen"

  if [[ "$version_out" != "7" ]]; then
    echo "::error::expected fastly-version=7 out of the action, got '${version_out:-<empty>}'" >&2
    return 1
  fi

  # The provider-env boundary: typed values in, inherited aliases out.
  local expected
  for expected in 'token=dummy-token' 'service-id=dummy-service' 'endpoint=CLEARED'; do
    if ! grep -qx -- "$expected" "$env_seen"; then
      echo "::error::provider-env boundary violated: expected '$expected' in env-seen.txt" >&2
      return 1
    fi
  done

  echo "production deploy, version threading, and the credential boundary all hold"
}

main "$@"
