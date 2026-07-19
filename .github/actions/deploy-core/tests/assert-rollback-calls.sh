#!/usr/bin/env bash
set -euo pipefail

# Asserts the rollback verbs, paths, and explicit-target threading:
#   * Activate/deactivate use PUT (the Fastly API rejects POST).
#   * Staging rollback deactivates on the `staging` environment
#     (`/deactivate/staging`), not the live one (`/deactivate`).
#   * Production activates the EXPLICIT `rollback-to`, never the staged version:
#     Fastly exposes no field to infer the previously-live version, so it must be
#     passed in (rollback-to=40 here) rather than computed.
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
  # The EXPLICIT rollback-to the smoke passes to the production rollback (it rolls
  # back FROM the active version 40 TO 39; the compare-and-swap guard requires the
  # rolled-back-from version to still be active).
  local target=39

  echo "--- recorded fastly/curl calls:"
  cat "$log"

  grep -qE "^PUT $api/version/$staged/deactivate/staging\$" "$log" ||
    fail "staging rollback did not PUT /version/$staged/deactivate/staging"

  # Production activates the EXPLICIT target, never the staged version (which is
  # what a `version - 1` inference would have picked here).
  if grep -qE "^PUT $api/version/$staged/activate\$" "$log"; then
    fail "production rollback activated the STAGED version $staged"
  fi
  grep -qE "^PUT $api/version/$target/activate\$" "$log" ||
    fail "production rollback did not PUT /version/$target/activate (the explicit rollback-to)"

  [[ "$rolled_back_to" == "$target" ]] ||
    fail "expected rolled-back-to=$target, got '${rolled_back_to:-<empty>}'"

  notice "rollback used PUT with the correct paths; rolled-back-to=$target threaded out"
}

main "$@"
