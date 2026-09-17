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
  local key='EDGEZERO__SERVICES__dummyservice__VERSIONS__42__ENV_V1'
  local descriptor="${FAKE_DESCRIPTOR_DIR:?FAKE_DESCRIPTOR_DIR is required}/$key"
  local fixture_mode="${EDGEZERO__TEST__FIXTURE_MODE:-store-aware}"

  [[ "$version" == 42 ]] || fail "expected production fastly-version=42, got '${version:-<empty>}'"
  [[ "$previous" == 40 ]] || fail "expected captured previous-version=40, got '${previous:-<empty>}'"
  [[ "$digest" == "$expected_digest" ]] || fail "production did not report the pinned package digest"
  [[ "$(cat "$FAKE_PACKAGE_DIGEST_FILE")" == "$expected_digest" ]] ||
    fail "production did not upload the pinned package bytes"

  local update_count expected_comment
  update_count=$(grep -Ec '^fastly compute update ' "$log" || true)
  [[ "$update_count" -eq 1 ]] || fail "production must issue exactly one compute update"
  grep -Eq '^fastly compute update --service-id=dummyservice --autoclone --version=active --package=[^[:space:]]+/package/app\.tar\.gz --non-interactive$' "$log" ||
    fail "production compute update did not use the exact active-source package command"
  case "$fixture_mode" in
    store-aware) expected_comment='production smoke' ;;
    store-free) expected_comment='store-free managed smoke' ;;
    *) fail "unknown production fixture mode '$fixture_mode'" ;;
  esac
  [[ "$(grep -Ec '^fastly service-version update ' "$log" || true)" -eq 1 ]] ||
    fail "production must issue exactly one version comment update"
  grep -Fqx "fastly service-version update --service-id=dummyservice --version=42 --comment $expected_comment" "$log" ||
    fail "production did not apply the exact version 42 comment"

  [[ -f "$descriptor" ]] || fail "production descriptor $key was not created"
  local expected_descriptor expected_links
  case "$fixture_mode" in
    store-aware)
      expected_descriptor='{"format":1,"entries":{"EDGEZERO__LOGGING__LEVEL":"info","EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY":"app_config","EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME":"config-prod","EDGEZERO__STORES__KV__CACHE__NAME":"cache-prod","EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME":"credentials-prod"}}'
      expected_links='[{"alias":"cache-prod","resource":"KVPROD"},{"alias":"config-prod","resource":"CONFIGPROD"},{"alias":"credentials-prod","resource":"SECRETPROD"},{"alias":"edgezero_runtime_env","resource":"ENVSEL1"}]'
      ;;
    store-free)
      expected_descriptor='{"format":1,"entries":{"EDGEZERO__LOGGING__LEVEL":"warn"}}'
      expected_links='[{"alias":"edgezero_runtime_env","resource":"ENVSEL1"}]'
      ;;
    *) fail "unknown production fixture mode '$fixture_mode'" ;;
  esac
  cmp -s "$descriptor" <(printf '%s' "$expected_descriptor") ||
    fail "production descriptor bytes are not the exact canonical runtime descriptor"

  jq -Rn --argjson expected "$expected_links" '
    [inputs | split("\t") | {alias: .[1], resource: .[2]}] |
    sort_by(.alias) == ($expected | sort_by(.alias))
  ' <"$FAKE_LINK_DIR/version-42.tsv" | grep -qx true ||
    fail "production links are not the exact selected resources"

  local resource_mutations expected_mutations
  resource_mutations=$(grep -E '^fastly resource-link (create|delete) ' "$log" || true)
  if [[ "$fixture_mode" == store-aware ]]; then
    [[ -z "$resource_mutations" ]] ||
      fail "store-aware production unexpectedly changed inherited matching resource links"
  else
    expected_mutations=$(cat <<'EOF'
fastly resource-link delete --service-id=dummyservice --version=42 --id=LINK_CONFIG_PROD
fastly resource-link delete --service-id=dummyservice --version=42 --id=LINK_KV_PROD
fastly resource-link delete --service-id=dummyservice --version=42 --id=LINK_SECRET_PROD
EOF
)
    [[ "$resource_mutations" == "$expected_mutations" ]] ||
      fail "store-free production did not delete exactly the inherited managed links"
  fi

  grep -q '^PUT https://api.fastly.com/service/dummyservice/version/42/activate$' "$log" ||
    fail "production did not activate prepared version 42"
  local create_line descriptor_read_line final_links_line activation_line last_delete_line
  create_line=$(grep -n "config-store-entry create --store-id=ENVSEL1 --key=$key" "$log" | tail -n 1 | cut -d: -f1)
  descriptor_read_line=$(grep -n "GET https://api.fastly.com/resources/stores/config/ENVSEL1/item/$key" "$log" | tail -n 1 | cut -d: -f1)
  final_links_line=$(grep -n '^fastly resource-link list --service-id=dummyservice --version=42 --json$' "$log" | tail -n 1 | cut -d: -f1)
  activation_line=$(grep -n '^PUT https://api.fastly.com/service/dummyservice/version/42/activate$' "$log" | tail -n 1 | cut -d: -f1)
  [[ "$create_line" -lt "$descriptor_read_line" && "$descriptor_read_line" -lt "$activation_line" ]] ||
    fail "descriptor create and exact readback did not precede production activation"
  [[ "$final_links_line" -lt "$activation_line" ]] ||
    fail "final link verification did not precede production activation"
  if [[ "$fixture_mode" == store-free ]]; then
    last_delete_line=$(grep -n '^fastly resource-link delete ' "$log" | tail -n 1 | cut -d: -f1)
    [[ "$last_delete_line" -lt "$final_links_line" ]] ||
      fail "store-free inherited-link deletion did not precede final verification"
  fi

  if grep -qE '^fastly config-store-entry describe ' "$log"; then
    fail "production issued a legacy Config Store exact-key describe"
  fi
  local unexpected_gets
  unexpected_gets=$(grep -E '^GET https://api\.fastly\.com/resources/stores/config/[^/]+/item/' "$log" |
    grep -Fvx \
      -e 'GET https://api.fastly.com/resources/stores/config/ENVSEL1/item/EDGEZERO__SERVICES__dummyservice__VERSIONS__40__ENV_V1' \
      -e "GET https://api.fastly.com/resources/stores/config/ENVSEL1/item/$key" || true)
  [[ -z "$unexpected_gets" ]] || fail "production issued a legacy scoped or unscoped exact-key read"
  if grep -Eq 'service-version stage|^fastly config-store-entry (update|delete) ' "$log"; then
    fail "production issued a staging or legacy selector command"
  fi
  notice "production activated version 42 with the pinned package and descriptor"
}

main "$@"
