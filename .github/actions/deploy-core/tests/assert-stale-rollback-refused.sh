#!/usr/bin/env bash
set -euo pipefail

# Asserts a STALE production rollback is refused and mutates NOTHING.
#
# A rollback workflow can run long after its deploy. If a newer version became
# active meanwhile, activating the old target would clobber that newer deploy, so
# the rollback must fail — and crucially must NOT have issued ANY activate PUT.
# (The check is best-effort, not atomic: it cannot close the window between the
# read and the activation. Service-scoped serialization is what does that.)
#
# To avoid passing for the WRONG reason (an artifact-download or CLI-startup
# failure would also produce a failed outcome with no PUT), this inspects only
# the DELTA of calls the stale rollback made and requires positive evidence the
# staleness guard actually ran: the active-version GET must be present, no
# activate PUT may appear, and the fake API's active version must still be 99.
#
# Reads (env):
#   FAKE_CALL_LOG                 required  the fake fastly/curl call log
#   FAKE_ACTIVE_VERSION_FILE      required  the fake API's active-version state
#   EDGEZERO__TEST__OUTCOME       required  the stale rollback step's outcome
#   EDGEZERO__TEST__LOG_SNAPSHOT  required  call-log line count BEFORE the rollback

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local state="${FAKE_ACTIVE_VERSION_FILE:?FAKE_ACTIVE_VERSION_FILE is required}"
  local outcome="${EDGEZERO__TEST__OUTCOME:?EDGEZERO__TEST__OUTCOME is required}"
  local snapshot="${EDGEZERO__TEST__LOG_SNAPSHOT:?EDGEZERO__TEST__LOG_SNAPSHOT is required}"
  local api='https://api\.fastly\.com/service/dummy-service'

  [[ "$outcome" == "failure" ]] ||
    fail "a stale production rollback must fail (the rolled-back-from version is no longer active), got outcome=$outcome"

  # Only the calls the stale rollback itself made.
  local delta
  delta=$(tail -n "+$((snapshot + 1))" "$log")
  echo "--- stale-rollback call delta:"
  printf '%s\n' "$delta"

  # Positive evidence the staleness guard ran (not an earlier startup failure):
  # it must have READ the active version.
  grep -qE "^GET $api/version\$" <<<"$delta" ||
    fail "the stale rollback never issued the active-version GET — it may have failed BEFORE the staleness check (download/startup), not because of it"

  # And it must NOT have activated ANYTHING — refusal precedes every mutation.
  if grep -qE "^PUT $api/version/[0-9]+/activate\$" <<<"$delta"; then
    fail "a stale rollback issued an activate PUT; it must refuse before mutating"
  fi

  # The active version is unchanged: the newer deploy (99) was not clobbered.
  local active
  active=$(cat "$state")
  [[ "$active" == "99" ]] ||
    fail "the active version changed to '$active'; a stale rollback must leave it at 99"

  notice "stale production rollback read the active version, refused, and activated nothing (active still 99)"
}

main "$@"
