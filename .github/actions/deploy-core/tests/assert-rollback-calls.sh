#!/usr/bin/env bash
set -euo pipefail

# Asserts the rollback verbs, paths, and version threading.
#
# Regression test for real defects a review found:
#   * Rollback used POST; the Fastly API requires PUT.
#   * Staging rollback hit `/deactivate`, which deactivates the LIVE version.
#     Undoing a stage is `/deactivate/staging`.
#
# Reads (env):
#   FAKE_CALL_LOG                         required  the fake fastly/curl call log
#   EDGEZERO__TEST__STAGED_VERSION        required  the version the staged deploy produced
#   EDGEZERO__TEST__ROLLED_BACK_TO        required  the production rollback's rolled-back-to output

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local staged="${EDGEZERO__TEST__STAGED_VERSION:?EDGEZERO__TEST__STAGED_VERSION is required}"
  local rolled_back_to="${EDGEZERO__TEST__ROLLED_BACK_TO:-}"
  local api='https://api\.fastly\.com/service/dummy-service'
  # The RESOLVED previously-live version, NOT staged-1. The fake version list has
  # v42 and v41 both staged and v40 active, so staged-1 (=41) would be a staged
  # version — the Critical this regression-tests. Only resolution lands on 40.
  local resolved=40

  echo "--- recorded fastly/curl calls:"
  cat "$log"

  grep -qE "^PUT $api/version/$staged/deactivate/staging\$" "$log" ||
    fail "staging rollback did not PUT /version/$staged/deactivate/staging"

  # Production rollback resolves the target from the version list first.
  grep -qE "^GET $api/version\$" "$log" ||
    fail "production rollback did not GET the version list to resolve its target"
  # ...and must NOT activate staged-1 (that would promote a staged version).
  if grep -qE "^PUT $api/version/$((staged - 1))/activate\$" "$log"; then
    fail "production rollback activated staged-1 ($((staged - 1))), a STAGED version — the Critical"
  fi
  grep -qE "^PUT $api/version/$resolved/activate\$" "$log" ||
    fail "production rollback did not PUT /version/$resolved/activate (the resolved previously-live version)"

  [[ "$rolled_back_to" == "$resolved" ]] ||
    fail "expected rolled-back-to=$resolved, got '${rolled_back_to:-<empty>}'"

  notice "rollback used PUT with the correct paths; rolled-back-to=$resolved threaded out"
}

main "$@"
