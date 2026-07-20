#!/usr/bin/env bash
set -euo pipefail

# Asserts a STALE production rollback is refused and mutates NOTHING.
#
# A rollback workflow can run long after its deploy. If a newer version became
# active meanwhile, activating the old target would clobber that newer deploy, so
# the rollback must fail — and crucially must NOT have issued the activate PUT.
# (The check is best-effort, not atomic: it cannot close the window between the
# read and the activation. Service-scoped serialization is what does that.)
#
# Reads (env):
#   FAKE_CALL_LOG                         required  the fake fastly/curl call log
#   EDGEZERO__TEST__OUTCOME               required  the stale rollback step's outcome
#   EDGEZERO__TEST__ROLLBACK_TO           required  the target it must NOT have activated

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local outcome="${EDGEZERO__TEST__OUTCOME:?EDGEZERO__TEST__OUTCOME is required}"
  local target="${EDGEZERO__TEST__ROLLBACK_TO:?EDGEZERO__TEST__ROLLBACK_TO is required}"
  local api='https://api\.fastly\.com/service/dummy-service'

  [[ "$outcome" == "failure" ]] ||
    fail "a stale production rollback must fail (the rolled-back-from version is no longer active), got outcome=$outcome"

  # The refusal must happen BEFORE any mutation: no activate for the target.
  if grep -qE "^PUT $api/version/$target/activate\$" "$log"; then
    fail "a stale rollback activated version $target -- it must refuse before mutating"
  fi

  notice "stale production rollback was refused and activated nothing"
}

main "$@"
