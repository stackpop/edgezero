#!/usr/bin/env bash
set -euo pipefail

# Asserts the real production-rollback wiring: a production rollback consumes the
# deploy's `previous-version` output as `rollback-to` (no hardcoded version), and
# the version it activates threads back out as `rolled-back-to`. The fake Fastly
# API reports version 40 active before the deploy, so both must be 40.
#
# Reads (env):
#   EDGEZERO__TEST__ROLLED_BACK_TO        required  the rollback's rolled-back-to output
#   EDGEZERO__TEST__PREVIOUS_VERSION      required  the deploy's captured previous-version

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local rolled_back_to="${EDGEZERO__TEST__ROLLED_BACK_TO:-}"
  local previous_version="${EDGEZERO__TEST__PREVIOUS_VERSION:-}"

  [[ "$rolled_back_to" == "$previous_version" ]] ||
    fail "the rollback must activate the captured previous-version; rolled-back-to='${rolled_back_to:-<empty>}' != previous-version='${previous_version:-<empty>}'"
  [[ "$rolled_back_to" == "40" ]] ||
    fail "expected the captured rollback target to be 40, got '${rolled_back_to:-<empty>}'"

  notice "production rollback activated the captured previous-version ($rolled_back_to)"
}

main "$@"
