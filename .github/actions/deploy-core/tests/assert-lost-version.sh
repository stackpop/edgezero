#!/usr/bin/env bash
set -euo pipefail

# The lost-version deploy must FAIL (no version to thread) yet still signal that a
# mutation may have occurred, so an operator knows to reconcile. It must also have
# actually reached the provider deploy command (the fixture records that).
#
# Reads (env):
#   GITHUB_WORKSPACE
#   EDGEZERO__TEST__DEPLOY_OUTCOME       the deploy step's outcome
#   EDGEZERO__TEST__MUTATION_ATTEMPTED   the deploy's mutation-attempted output
#   EDGEZERO__TEST__PREVIOUS_VERSION     the deploy's previous-version output

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local outcome="${EDGEZERO__TEST__DEPLOY_OUTCOME:-}"
  local mutation="${EDGEZERO__TEST__MUTATION_ATTEMPTED:-}"
  local previous="${EDGEZERO__TEST__PREVIOUS_VERSION:-}"

  [[ "$outcome" == "failure" ]] ||
    fail "the lost-version deploy should have FAILED, but its outcome was '$outcome'"

  [[ "$mutation" == "true" ]] ||
    fail "a failed-but-mutating deploy must still emit mutation-attempted=true, got '$mutation'"

  # The rollback target captured before the deploy must survive the failure so
  # recovery can thread it — the smoke rolls back to exactly this value.
  [[ "$previous" == "40" ]] ||
    fail "the failed deploy must still expose previous-version=40 (the captured rollback target), got '${previous:-<empty>}'"

  # The deploy really reached the provider command (which recorded the env it saw).
  [[ -f "$workspace/fixture-app/env-seen.txt" ]] ||
    fail "the deploy never reached the app CLI's Fastly deploy command"

  notice "lost-version deploy failed as expected, with mutation-attempted=true"
}

main "$@"
