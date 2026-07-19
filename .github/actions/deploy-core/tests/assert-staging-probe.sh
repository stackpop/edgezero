#!/usr/bin/env bash
set -euo pipefail

# Asserts the staging health check resolved the staged version's IP and probed
# through it.
#
# Regression test: the Fastly domain API returns a SINGULAR `staging_ip` string
# per domain object (`staging_ips` is only the `include=` query param). Reading
# it as an array silently found no IP and probed PRODUCTION instead — a staging
# check that was quietly testing the wrong thing.
#
# Reads (env):
#   FAKE_CALL_LOG                         required  the fake fastly/curl call log
#   EDGEZERO__TEST__STAGED_VERSION        required  the version the staged deploy produced
#   EDGEZERO__TEST__HEALTHY               required  the healthcheck's `healthy` output
#   EDGEZERO__TEST__STATUS_CODE           required  the healthcheck's `status-code` output

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local staged="${EDGEZERO__TEST__STAGED_VERSION:?EDGEZERO__TEST__STAGED_VERSION is required}"
  local healthy="${EDGEZERO__TEST__HEALTHY:-}"
  local status_code="${EDGEZERO__TEST__STATUS_CODE:-}"

  grep -qE "^GET https://api\.fastly\.com/service/dummy-service/version/$staged/domain\?include=staging_ips\$" "$log" ||
    fail "the staging-IP lookup was never performed for version $staged"

  grep -qE '^PROBE .*--connect-to ::151\.101\.2\.10:443 .*https://staging\.example\.com/' "$log" ||
    fail "the probe was not rerouted to the staging IP (was the singular staging_ip read?)"

  # The public outputs must reflect a healthy probe: the fake curl returns 200,
  # so a passing staged healthcheck must thread healthy=true and status-code=200.
  [[ "$healthy" == "true" ]] ||
    fail "expected the healthcheck output healthy=true, got '${healthy:-<empty>}'"
  [[ "$status_code" == "200" ]] ||
    fail "expected the healthcheck output status-code=200, got '${status_code:-<empty>}'"

  notice "staging probe was rerouted through the resolved staging IP (healthy=true, status-code=200)"
}

main "$@"
