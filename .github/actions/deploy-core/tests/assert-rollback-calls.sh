#!/usr/bin/env bash
set -euo pipefail

# Asserts the rollback verbs, paths, and version threading.
#
# Regression test for real defects a review found:
#   * Rollback used POST; the Fastly API requires PUT.
#   * Staging rollback hit `/deactivate`, which deactivates the LIVE version.
#     Undoing a stage is `/deactivate/staging`.
#
# Reads (env): FAKE_CALL_LOG, EDGEZERO__TEST__STAGED_VERSION, EDGEZERO__TEST__ROLLED_BACK_TO.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local staged="${EDGEZERO__TEST__STAGED_VERSION:?EDGEZERO__TEST__STAGED_VERSION is required}"
  local rolled_back_to="${EDGEZERO__TEST__ROLLED_BACK_TO:-}"
  local api='https://api\.fastly\.com/service/dummy-service'
  local previous=$((staged - 1))

  echo "--- recorded fastly/curl calls:"
  cat "$log"

  grep -qE "^PUT $api/version/$staged/deactivate/staging\$" "$log" ||
    fail "staging rollback did not PUT /version/$staged/deactivate/staging"

  # Production rollback activates the PREVIOUS version.
  grep -qE "^PUT $api/version/$previous/activate\$" "$log" ||
    fail "production rollback did not PUT /version/$previous/activate"

  [[ "$rolled_back_to" == "$previous" ]] ||
    fail "expected rolled-back-to=$previous, got '${rolled_back_to:-<empty>}'"

  notice "rollback used PUT with the correct paths; rolled-back-to=$previous threaded out"
}

main "$@"
