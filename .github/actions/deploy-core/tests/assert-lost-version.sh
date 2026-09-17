#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local outcome="${EDGEZERO__TEST__DEPLOY_OUTCOME:-}"
  local mutation="${EDGEZERO__TEST__MUTATION_ATTEMPTED:-}"
  local previous="${EDGEZERO__TEST__PREVIOUS_VERSION:-}"
  local version="${EDGEZERO__TEST__FASTLY_VERSION:-}"
  local digest="${EDGEZERO__TEST__PACKAGE_DIGEST:-}"
  local expected_digest="${FAKE_EXPECTED_PACKAGE_DIGEST:?FAKE_EXPECTED_PACKAGE_DIGEST is required}"
  [[ "$outcome" == failure ]] || fail "the post-upload provider failure unexpectedly succeeded"
  [[ "$mutation" == true && "$previous" == 40 && "$version" == 42 ]] || fail "the failed deploy did not retain recovery outputs"
  [[ "$digest" == "$expected_digest" && "$(cat "$FAKE_PACKAGE_DIGEST_FILE")" == "$expected_digest" ]] || fail "the failed deploy did not retain its verified package digest"
  grep -q '^fastly compute update ' "$FAKE_CALL_LOG" || fail "failure occurred before package upload"
  grep -q '^fastly resource-link delete ' "$FAKE_CALL_LOG" || fail "failure did not occur during post-upload reconciliation"
  ! grep -Eq 'service-version stage|/version/42/activate' "$FAKE_CALL_LOG" || fail "the failed deployment published version 42"
  ! grep -qE 'config-store-entry (create|describe)|/resources/stores/config/.*/item/' "$FAKE_CALL_LOG" || fail "the failed deployment used the removed runtime descriptor path"
  notice "failed deploy retained version 42 and the verified package digest"
}
main "$@"
