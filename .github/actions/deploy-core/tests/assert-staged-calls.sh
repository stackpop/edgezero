#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local descriptor_dir="${FAKE_DESCRIPTOR_DIR:?FAKE_DESCRIPTOR_DIR is required}"
  local link_dir="${FAKE_LINK_DIR:?FAKE_LINK_DIR is required}"
  local version="${EDGEZERO__TEST__STAGED_VERSION:-}"
  local digest="${EDGEZERO__TEST__PACKAGE_DIGEST:-}"
  local expected_digest="${FAKE_EXPECTED_PACKAGE_DIGEST:?FAKE_EXPECTED_PACKAGE_DIGEST is required}"
  local key='EDGEZERO__SERVICES__dummyservice__VERSIONS__42__ENV_V1'
  local descriptor="$descriptor_dir/$key"

  [[ "$version" == 42 ]] || fail "expected staged fastly-version=42, got '${version:-<empty>}'"
  [[ "$digest" == "$expected_digest" ]] || fail "staging did not report the pinned package digest"
  [[ "$(cat "$FAKE_PACKAGE_DIGEST_FILE")" == "$expected_digest" ]] ||
    fail "staging did not upload the pinned package bytes"

  [[ "$(grep -Ec '^fastly compute update ' "$log" || true)" -eq 1 ]] ||
    fail "staging must issue exactly one compute update"
  grep -Eq '^fastly compute update --service-id=dummyservice --autoclone --version=active --package=[^[:space:]]+/package/app\.tar\.gz --non-interactive$' "$log" ||
    fail "staging compute update did not use the exact active-source package command"
  grep -Eq '^fastly compute hash-files --package=[^[:space:]]+/package/app\.tar\.gz --skip-build --non-interactive --quiet$' "$log" ||
    fail "staging did not hash the pinned package with the exact command"
  [[ "$(grep -Ec '^fastly service-version update ' "$log" || true)" -eq 1 ]] ||
    fail "staging must issue exactly one version comment update"
  grep -Fqx 'fastly service-version update --service-id=dummyservice --version=42 --comment staged smoke' "$log" ||
    fail "the staged comment was not applied to version 42"

  [[ -f "$descriptor" ]] || fail "the exact version descriptor was not created"
  local expected_descriptor
  expected_descriptor='{"format":1,"entries":{"EDGEZERO__LOGGING__LEVEL":"debug","EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY":"app_config_staging","EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME":"config-stage","EDGEZERO__STORES__KV__CACHE__NAME":"cache-stage","EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME":"credentials-stage"}}'
  cmp -s "$descriptor" <(printf '%s' "$expected_descriptor") ||
    fail "the staging descriptor bytes are not the exact canonical runtime descriptor"

  jq -Rn '
    [inputs | split("\t") | {alias: .[1], resource: .[2]}] |
    sort_by(.alias) == ([
      {alias:"cache-stage", resource:"KVSTAGE"},
      {alias:"config-stage", resource:"CONFIGSTAGE"},
      {alias:"credentials-stage", resource:"SECRETSTAGE"},
      {alias:"edgezero_runtime_env", resource:"ENVSEL1"}
    ] | sort_by(.alias))
  ' <"$link_dir/version-42.tsv" | grep -qx true || fail "staging links are not the exact selected resources"

  local resource_mutations expected_mutations
  resource_mutations=$(grep -E '^fastly resource-link (create|delete) ' "$log" || true)
  expected_mutations=$(cat <<'EOF'
fastly resource-link delete --service-id=dummyservice --version=42 --id=LINK_KV_PROD
fastly resource-link delete --service-id=dummyservice --version=42 --id=LINK_CONFIG_PROD
fastly resource-link delete --service-id=dummyservice --version=42 --id=LINK_SECRET_PROD
fastly resource-link create --service-id=dummyservice --version=42 --resource-id=KVSTAGE --name=cache-stage
fastly resource-link create --service-id=dummyservice --version=42 --resource-id=CONFIGSTAGE --name=config-stage
fastly resource-link create --service-id=dummyservice --version=42 --resource-id=SECRETSTAGE --name=credentials-stage
EOF
  )
  if [[ "$resource_mutations" != "$expected_mutations" ]]; then
    printf 'expected resource mutations:\n%s\nactual resource mutations:\n%s\n' \
      "$expected_mutations" "${resource_mutations:-<none>}" >&2
    fail "staging did not replace exactly the inherited production links with staging links"
  fi

  grep -q "config-store-entry create --store-id=ENVSEL1 --key=$key --stdin" "$log" ||
    fail "the descriptor was not written under the exact version key"
  grep -q '^fastly service-version stage --service-id=dummyservice --version=42$' "$log" ||
    fail "prepared version 42 was not staged"

  local create_line descriptor_read_line final_links_line package_read_line stage_line
  local last_delete_line first_create_line last_create_line
  create_line=$(grep -n "config-store-entry create --store-id=ENVSEL1 --key=$key" "$log" | tail -n 1 | cut -d: -f1)
  descriptor_read_line=$(grep -n "GET https://api.fastly.com/resources/stores/config/ENVSEL1/item/$key" "$log" | tail -n 1 | cut -d: -f1)
  final_links_line=$(grep -n '^fastly resource-link list --service-id=dummyservice --version=42 --json$' "$log" | tail -n 1 | cut -d: -f1)
  package_read_line=$(grep -n '^GET https://api.fastly.com/service/dummyservice/version/42/package$' "$log" | tail -n 1 | cut -d: -f1)
  stage_line=$(grep -n '^fastly service-version stage --service-id=dummyservice --version=42$' "$log" | cut -d: -f1)
  [[ "$create_line" -lt "$descriptor_read_line" && "$descriptor_read_line" -lt "$stage_line" ]] ||
    fail "descriptor create/readback/final verification did not precede staging"
  [[ "$final_links_line" -lt "$stage_line" ]] || fail "final link verification did not precede staging"
  [[ "$package_read_line" -lt "$stage_line" ]] ||
    fail "final package identity verification did not precede staging"
  last_delete_line=$(grep -n '^fastly resource-link delete ' "$log" | tail -n 1 | cut -d: -f1)
  first_create_line=$(grep -n '^fastly resource-link create ' "$log" | head -n 1 | cut -d: -f1)
  last_create_line=$(grep -n '^fastly resource-link create ' "$log" | tail -n 1 | cut -d: -f1)
  [[ "$last_delete_line" -lt "$first_create_line" && "$last_create_line" -lt "$final_links_line" ]] ||
    fail "production-link deletion, staging-link creation, and final verification were out of order"

  if grep -qE '^fastly config-store-entry describe ' "$log"; then
    fail "staging issued a legacy Config Store exact-key describe"
  fi
  local unexpected_gets
  unexpected_gets=$(grep -E '^GET https://api\.fastly\.com/resources/stores/config/[^/]+/item/' "$log" |
    grep -Fvx \
      -e 'GET https://api.fastly.com/resources/stores/config/ENVSEL1/item/EDGEZERO__SERVICES__dummyservice__VERSIONS__40__ENV_V1' \
      -e "GET https://api.fastly.com/resources/stores/config/ENVSEL1/item/$key" || true)
  [[ -z "$unexpected_gets" ]] || fail "staging issued a legacy scoped or unscoped exact-key read"
  if grep -Eq '^fastly config-store-entry (update|delete) ' "$log"; then
    fail "a legacy selector or staging-twin command was issued"
  fi
  notice "staged version 42 uses the pinned package and version-scoped descriptor"
}

main "$@"
