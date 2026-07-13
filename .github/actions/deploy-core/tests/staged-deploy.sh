#!/usr/bin/env bash
set -euo pipefail

# Runs a STAGED deploy through the app-owned CLI against the fake `fastly`
# (see make-fake-fastly-env.sh) and asserts the staged version is emitted.
#
# The version parser is fail-closed, so an empty `version=` here means the
# adapter failed to read the version back out of `fastly compute update`.
#
# Inputs (environment): GITHUB_WORKSPACE, CLI_BIN, FASTLY_API_TOKEN,
# FASTLY_SERVICE_ID.

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local cli_bin="${CLI_BIN:?CLI_BIN is required}"
  local out

  cd "$workspace/fixture-app"

  out=$("$workspace/cli-bin/$cli_bin" deploy \
    --adapter fastly \
    --service-id dummy-service \
    --stage \
    -- --comment "staged smoke" 2>&1 | tee /dev/stderr)

  if ! printf '%s\n' "$out" | grep -qE '^version=42$'; then
    echo "::error::staged deploy did not emit version=42" >&2
    return 1
  fi
  echo "staged deploy emitted version=42"
}

main "$@"
