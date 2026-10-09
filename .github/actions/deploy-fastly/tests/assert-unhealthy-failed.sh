#!/usr/bin/env bash
set -euo pipefail

# Asserts healthcheck-fastly FAILS CLOSED on an unhealthy probe.
#
# Callers gate their rollback on "the healthcheck step failed". If an unhealthy
# probe lets the action succeed, no caller would ever roll back — so this is the
# single most important contract in the lifecycle.
#
# Reads (env):
#   EDGEZERO__TEST__OUTCOME               required  the healthcheck step's outcome (success/failure)
#   EDGEZERO__TEST__HEALTHY               required  the healthcheck step's healthy output
#   EDGEZERO__TEST__STATUS_CODE           required  the healthcheck step's status-code output

main() {
  local outcome="${EDGEZERO__TEST__OUTCOME:?EDGEZERO__TEST__OUTCOME is required}"
  local healthy="${EDGEZERO__TEST__HEALTHY:-}"
  local status_code="${EDGEZERO__TEST__STATUS_CODE:-}"

  if [[ "$outcome" != "failure" ]]; then
    echo "::error::an unhealthy probe did not fail healthcheck-fastly (outcome=$outcome)" >&2
    return 1
  fi
  # The verdict must be an EXPLICIT `false`, not merely "not true" — an empty
  # output would mean the step failed without emitting a verdict at all.
  if [[ "$healthy" != "false" ]]; then
    echo "::error::an unhealthy probe must report healthy=false, got '${healthy:-<empty>}'" >&2
    return 1
  fi
  # The fake probe returns 503 when FORCE_UNHEALTHY is set; the status-code must
  # thread out so callers can see WHY it was unhealthy.
  if [[ "$status_code" != "503" ]]; then
    echo "::error::an unhealthy probe must report status-code=503, got '${status_code:-<empty>}'" >&2
    return 1
  fi

  echo "healthcheck-fastly failed closed on an unhealthy probe (healthy=false, status-code=503)"
}

main "$@"
