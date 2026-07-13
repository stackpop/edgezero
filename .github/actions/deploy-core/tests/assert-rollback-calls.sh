#!/usr/bin/env bash
set -euo pipefail

# Asserts the rollback verbs, paths, and version threading.
#
# Regression test for two real defects a review found:
#   * Rollback used POST; the Fastly API requires PUT.
#   * Staging rollback hit `/deactivate`, which deactivates the LIVE version.
#     Undoing a stage is `/deactivate/staging`.
#
# Inputs (environment): FAKE_CALL_LOG, ROLLED_BACK_TO.

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local rolled_back_to="${ROLLED_BACK_TO:-}"
  local api="https://api\.fastly\.com/service/dummy-service"

  echo "--- recorded fastly/curl calls:"
  cat "$log"

  if ! grep -qE "^PUT $api/version/42/deactivate/staging\$" "$log"; then
    echo "::error::staging rollback did not PUT /version/42/deactivate/staging" >&2
    return 1
  fi

  # Production rollback activates the PREVIOUS version (42 -> 41).
  if ! grep -qE "^PUT $api/version/41/activate\$" "$log"; then
    echo "::error::production rollback did not PUT /version/41/activate" >&2
    return 1
  fi

  if [[ "$rolled_back_to" != "41" ]]; then
    echo "::error::expected rolled-back-to=41, got '${rolled_back_to:-<empty>}'" >&2
    return 1
  fi

  echo "rollback used PUT with the correct paths, and rolled-back-to=41 threaded out"
}

main "$@"
