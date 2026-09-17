#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local version="${EDGEZERO__TEST__FASTLY_VERSION:-}"
  local previous="${EDGEZERO__TEST__PREVIOUS_VERSION:-}"
  local digest="${EDGEZERO__TEST__PACKAGE_DIGEST:-}"
  local expected_digest="${FAKE_EXPECTED_PACKAGE_DIGEST:?FAKE_EXPECTED_PACKAGE_DIGEST is required}"
  local fixture_mode="${EDGEZERO__TEST__FIXTURE_MODE:-store-aware}"

  [[ "$version" == 42 ]] || fail "expected production fastly-version=42, got '${version:-<empty>}'"
  [[ "$previous" == 40 ]] || fail "expected captured previous-version=40, got '${previous:-<empty>}'"
  [[ "$digest" == "$expected_digest" ]] || fail "production did not report the pinned package digest"
  [[ "$(cat "$FAKE_PACKAGE_DIGEST_FILE")" == "$expected_digest" ]] || fail "production uploaded different package bytes"
  grep -Eq '^fastly compute update --service-id=dummyservice --autoclone --version=active --package=[^[:space:]]+/package/app\.tar\.gz --non-interactive$' "$log" || fail "production did not use the exact package update command"
  grep -Eq '^fastly compute hash-files --package=[^[:space:]]+/package/app\.tar\.gz --skip-build --non-interactive --quiet$' "$log" || fail "production did not hash the pinned package"

  local expected_comment
  case "$fixture_mode" in
    store-aware) expected_comment='production smoke' ;;
    store-free) expected_comment='store-free managed smoke' ;;
    *) fail "unknown production fixture mode '$fixture_mode'" ;;
  esac
  grep -Fqx "fastly service-version update --service-id=dummyservice --version=42 --comment $expected_comment" "$log" || fail "production did not apply the exact version comment"

  jq -Rn '
    [inputs | split("\t") | {alias: .[1], resource: .[2], type: .[3]}] |
    sort_by(.type, .alias) == ([
      {alias:"app_config", resource:"CONFIGPROD", type:"config_store"},
      {alias:"cache", resource:"KVPROD", type:"kv_store"},
      {alias:"credentials", resource:"SECRETPROD", type:"secret_store"}
    ] | sort_by(.type, .alias))
  ' <"$FAKE_LINK_DIR/version-42.tsv" | grep -qx true || fail "production links do not expose the selected resources under logical aliases"

  local mutations
  mutations=$(grep -E '^fastly resource-link (create|delete) ' "$log" || true)
  [[ "$mutations" == 'fastly resource-link delete --service-id=dummyservice --version=42 --id=LINK_RUNTIME' ]] || fail "production must remove only the exact legacy runtime link; got: ${mutations:-<none>}"
  grep -q '^PUT https://api.fastly.com/service/dummyservice/version/42/activate$' "$log" || fail "production did not activate version 42"

  local delete_line final_links_line package_line publish_line
  delete_line=$(grep -n '^fastly resource-link delete ' "$log" | tail -n1 | cut -d: -f1)
  final_links_line=$(grep -n '^fastly resource-link list --service-id=dummyservice --version=42 --json$' "$log" | tail -n1 | cut -d: -f1)
  package_line=$(grep -n '^GET https://api.fastly.com/service/dummyservice/version/42/package$' "$log" | tail -n1 | cut -d: -f1)
  publish_line=$(grep -n '^PUT https://api.fastly.com/service/dummyservice/version/42/activate$' "$log" | tail -n1 | cut -d: -f1)
  [[ "$delete_line" -lt "$final_links_line" && "$final_links_line" -lt "$publish_line" && "$package_line" -lt "$publish_line" ]] || fail "final link and package verification did not follow reconciliation and precede activation"
  ! grep -qE 'config-store-entry (create|describe)|/resources/stores/config/.*/item/' "$log" || fail "production used the removed runtime descriptor path"
  notice "production activated version 42 with logical resource links and pinned package bytes"
}
main "$@"
