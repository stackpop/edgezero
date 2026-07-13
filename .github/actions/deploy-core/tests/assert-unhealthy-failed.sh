#!/usr/bin/env bash
set -euo pipefail

# Asserts healthcheck-fastly FAILS CLOSED on an unhealthy probe.
#
# Callers gate their rollback on "the healthcheck step failed". If an unhealthy
# probe lets the action succeed, no caller would ever roll back — so this is the
# single most important contract in the lifecycle.
#
# Inputs (environment): OUTCOME (the step's outcome), HEALTHY (its output).

main() {
  local outcome="${OUTCOME:?OUTCOME is required}"
  local healthy="${HEALTHY:-}"

  if [[ "$outcome" != "failure" ]]; then
    echo "::error::an unhealthy probe did not fail healthcheck-fastly (outcome=$outcome)" >&2
    return 1
  fi
  if [[ "$healthy" == "true" ]]; then
    echo "::error::an unhealthy probe still reported healthy=true" >&2
    return 1
  fi

  echo "healthcheck-fastly failed closed on an unhealthy probe"
}

main "$@"
