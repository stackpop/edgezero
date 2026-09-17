#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local version="${EDGEZERO__TEST__STAGED_VERSION:-}"
  local digest="${EDGEZERO__TEST__PACKAGE_DIGEST:-}"
  local expected_digest="${FAKE_EXPECTED_PACKAGE_DIGEST:?FAKE_EXPECTED_PACKAGE_DIGEST is required}"
  [[ "$version" == 42 ]] || fail "expected staged fastly-version=42, got '${version:-<empty>}'"
  [[ "$digest" == "$expected_digest" ]] || fail "staging did not report the pinned package digest"
  [[ "$(cat "$FAKE_PACKAGE_DIGEST_FILE")" == "$expected_digest" ]] || fail "staging uploaded different package bytes"

  jq -Rn '
    [inputs | split("\t") | {alias: .[1], resource: .[2], type: .[3]}] |
    sort_by(.type, .alias) == ([
      {alias:"app_config", resource:"CONFIGSTAGE", type:"config-store"},
      {alias:"cache", resource:"KVSTAGE", type:"object-store"},
      {alias:"credentials", resource:"SECRETSTAGE", type:"secret-store"}
    ] | sort_by(.type, .alias))
  ' <"$FAKE_LINK_DIR/version-42.tsv" | grep -qx true || fail "staging links do not expose staging resources under logical aliases"

  local expected mutations
  expected=$(cat <<'MUTATIONS'
fastly resource-link delete --service-id=dummyservice --version=42 --id=LINK_CONFIG_PROD
fastly resource-link delete --service-id=dummyservice --version=42 --id=LINK_KV_PROD
fastly resource-link delete --service-id=dummyservice --version=42 --id=LINK_SECRET_PROD
fastly resource-link create --service-id=dummyservice --version=42 --resource-id=CONFIGSTAGE --name=app_config
fastly resource-link create --service-id=dummyservice --version=42 --resource-id=KVSTAGE --name=cache
fastly resource-link create --service-id=dummyservice --version=42 --resource-id=SECRETSTAGE --name=credentials
MUTATIONS
)
  mutations=$(grep -E '^fastly resource-link (create|delete) ' "$log" || true)
  [[ "$mutations" == "$expected" ]] || { printf 'expected resource mutations:\n%s\nactual resource mutations:\n%s\n' "$expected" "${mutations:-<none>}" >&2; fail "staging reconciliation differed"; }
  grep -q '^fastly service-version stage --service-id=dummyservice --version=42$' "$log" || fail "version 42 was not staged"

  local last_create final_links package_line configuration_line stage_line
  last_create=$(grep -n '^fastly resource-link create ' "$log" | tail -n1 | cut -d: -f1)
  final_links=$(grep -n '^fastly resource-link list --service-id=dummyservice --version=42 --json$' "$log" | tail -n1 | cut -d: -f1)
  package_line=$(grep -n '^GET https://api.fastly.com/service/dummyservice/version/42/package$' "$log" | tail -n1 | cut -d: -f1)
  configuration_line=$(grep -n '^GET https://api.fastly.com/service/dummyservice/diff/from/42/to/42$' "$log" | tail -n1 | cut -d: -f1)
  stage_line=$(grep -n '^fastly service-version stage --service-id=dummyservice --version=42$' "$log" | tail -n1 | cut -d: -f1)
  [[ "$(grep -c '^GET https://api.fastly.com/service/dummyservice/diff/from/42/to/42$' "$log")" -eq 2 ]] || fail "staging did not capture and revalidate the complete draft configuration"
  [[ "$last_create" -lt "$final_links" && "$final_links" -lt "$stage_line" && "$package_line" -lt "$stage_line" && "$configuration_line" -lt "$stage_line" ]] || fail "final link, package, and complete-configuration verification did not follow reconciliation and precede staging"
  ! grep -qE 'config-store-entry (create|describe)|/resources/stores/config/.*/item/' "$log" || fail "staging used the removed runtime descriptor path"
  notice "staged version 42 uses logical resource links and pinned package bytes"
}
main "$@"
