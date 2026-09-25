#!/usr/bin/env bash
set -euo pipefail

# Asserts a failed deploy preserves every independently valid recovery output
# after Fastly committed activation but its response reported a failure.
#
# Reads (env):
#   EDGEZERO__TEST__DEPLOY_OUTCOME, EDGEZERO__TEST__MUTATION_ATTEMPTED
#   EDGEZERO__TEST__PREVIOUS_VERSION, EDGEZERO__TEST__FASTLY_VERSION
#   EDGEZERO__TEST__PACKAGE_DIGEST, FAKE_EXPECTED_PACKAGE_DIGEST
#   FAKE_PACKAGE_DIGEST_FILE, FAKE_ACTIVE_VERSION_FILE
# Writes: none

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../fastly-common/scripts/common.sh
source "$SCRIPT_DIR/../../fastly-common/scripts/common.sh"

main() {
  local outcome="${EDGEZERO__TEST__DEPLOY_OUTCOME:-}"
  local mutation="${EDGEZERO__TEST__MUTATION_ATTEMPTED:-}"
  local previous="${EDGEZERO__TEST__PREVIOUS_VERSION:-}"
  local version="${EDGEZERO__TEST__FASTLY_VERSION:-}"
  local digest="${EDGEZERO__TEST__PACKAGE_DIGEST:-}"
  local expected_digest="${FAKE_EXPECTED_PACKAGE_DIGEST:?FAKE_EXPECTED_PACKAGE_DIGEST is required}"
  [[ "$outcome" == failure ]] || fail "the ambiguous activation unexpectedly succeeded"
  [[ "$mutation" == true && "$previous" == 40 && "$version" == 42 ]] || fail "the failed deploy did not retain recovery outputs"
  [[ "$digest" == "$expected_digest" && "$(cat "$FAKE_PACKAGE_DIGEST_FILE")" == "$expected_digest" ]] || fail "the failed deploy did not retain its verified package digest"
  grep -q '^fastly compute update ' "$FAKE_CALL_LOG" || fail "failure occurred before package upload"
  grep -q '^PUT .*/service/dummyservice/version/42/activate$' "$FAKE_CALL_LOG" || fail "failure did not occur at activation"
  [[ "$(cat "$FAKE_ACTIVE_VERSION_FILE")" == 42 ]] || fail "activation did not commit before the failed response"
  ! grep -q 'service version stage' "$FAKE_CALL_LOG" || fail "production recovery smoke staged instead of activating"
  ! grep -qE 'config-store-entry (create|describe)|/resources/stores/config/.*/item/' "$FAKE_CALL_LOG" || fail "the failed deployment used the removed runtime descriptor path"
  notice "failed response retained version 42 while provider state confirms activation"
}
main "$@"
