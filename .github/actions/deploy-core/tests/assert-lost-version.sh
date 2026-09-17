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
  [[ "$mutation" == true ]] || fail "the failed deploy did not retain mutation-attempted=true"
  [[ "$previous" == 40 ]] || fail "the failed deploy did not retain previous-version=40"
  [[ "$version" == 42 ]] || fail "the failed deploy did not retain recoverable fastly-version=42"
  [[ "$digest" == "$expected_digest" ]] || fail "the failed deploy did not retain its package digest"
  [[ "$(cat "$FAKE_PACKAGE_DIGEST_FILE")" == "$expected_digest" ]] ||
    fail "the failed deployment uploaded different package bytes"

  grep -q '^fastly compute update ' "$FAKE_CALL_LOG" || fail "failure occurred before package upload"
  if grep -Eq 'service-version stage|/version/42/activate' "$FAKE_CALL_LOG"; then
    fail "the failed deployment published version 42"
  fi
  if grep -qE '^fastly config-store-entry describe ' "$FAKE_CALL_LOG"; then
    fail "the failed deployment issued a legacy Config Store exact-key describe"
  fi
  local unexpected_gets
  unexpected_gets=$(grep -E '^GET https://api\.fastly\.com/resources/stores/config/[^/]+/item/' "$FAKE_CALL_LOG" |
    grep -Fvx \
      -e 'GET https://api.fastly.com/resources/stores/config/ENVSEL1/item/EDGEZERO__SERVICES__dummyservice__VERSIONS__40__ENV_V1' \
      -e 'GET https://api.fastly.com/resources/stores/config/ENVSEL1/item/EDGEZERO__SERVICES__dummyservice__VERSIONS__42__ENV_V1' || true)
  [[ -z "$unexpected_gets" ]] ||
    fail "the failed deployment issued a legacy scoped or unscoped exact-key read"
  notice "failed deploy retained version 42 and the verified package digest"
}

main "$@"
