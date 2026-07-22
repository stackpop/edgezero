#!/usr/bin/env bash
set -euo pipefail

# Asserts a PRODUCTION healthcheck probes WITHOUT any provider token, even when
# one is inherited from the job environment.
#
# A production probe just curls the public domain — it needs no credential, so the
# wrapper passes none. This reads the delta of the fake call log (the calls this
# probe made) and requires the probe to have run with FASTLY_API_TOKEN absent.
# Without the delta, an earlier STAGING probe (which legitimately holds a token)
# would mask a regression here.
#
# Reads (env):
#   FAKE_CALL_LOG                 required  the fake fastly/curl call log
#   EDGEZERO__TEST__LOG_SNAPSHOT  required  call-log line count BEFORE the probe
#   EDGEZERO__TEST__HEALTHY       required  the healthcheck's `healthy` output

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local snapshot="${EDGEZERO__TEST__LOG_SNAPSHOT:?EDGEZERO__TEST__LOG_SNAPSHOT is required}"
  local healthy="${EDGEZERO__TEST__HEALTHY:-}"

  local delta
  delta=$(tail -n "+$((snapshot + 1))" "$log")
  echo "--- production-probe call delta:"
  printf '%s\n' "$delta"

  # The probe must actually have run (otherwise "no token" is vacuous).
  grep -qE '^PROBE ' <<<"$delta" ||
    fail "the production healthcheck never issued a probe"

  # Every probe in the delta must have run with NO token in scope.
  if grep -qE '^PROBE-TOKEN=set$' <<<"$delta"; then
    fail "a production probe ran with FASTLY_API_TOKEN in scope; a production healthcheck must receive no token even when one is inherited from the job env"
  fi
  grep -qE '^PROBE-TOKEN=$' <<<"$delta" ||
    fail "the production probe recorded no token state — the fake curl did not run as expected"

  # And it still worked (the token is genuinely unnecessary for production).
  [[ "$healthy" == "true" ]] ||
    fail "the production healthcheck should succeed without a token, got healthy='${healthy:-<empty>}'"

  notice "production healthcheck probed with NO provider token and reported healthy"
}

main "$@"
