#!/usr/bin/env bash
set -euo pipefail

# After recovery: active-version found the live version (7), and rollback-fastly
# re-activated the captured previous version (40). Assert the rollback threaded that
# target back out, and that the fake service is now actually at 40.
#
# Reads (env):
#   GITHUB_WORKSPACE
#   FAKE_ACTIVE_VERSION_FILE           the fake service's current active version
#   EDGEZERO__TEST__ROLLED_BACK_TO     rollback-fastly's rolled-back-to output

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local rolled_back_to="${EDGEZERO__TEST__ROLLED_BACK_TO:-}"
  local active_file="${FAKE_ACTIVE_VERSION_FILE:-}"

  [[ "$rolled_back_to" == "40" ]] ||
    fail "expected rolled-back-to=40 (the captured previous version), got '${rolled_back_to:-<empty>}'"

  local active=""
  [[ -n "$active_file" && -f "$active_file" ]] && active=$(cat "$active_file")
  [[ "$active" == "40" ]] ||
    fail "the fake service should be back at version 40 after rollback, but it is at '${active:-<empty>}'"

  notice "recovery complete: active-version recovered the live version and rollback restored 40"
}

main "$@"
