#!/usr/bin/env bash
set -euo pipefail

# Rolls a Fastly deployment back through the application CLI.
#
# Production activates the previous version; staging deactivates the staged one.
# Fails closed: a rollback that cannot say what it activated has not provably
# rolled anything back.
#
# Inputs (environment): EDGEZERO__CLI__BIN, EDGEZERO__LIFECYCLE__SERVICE_ID, EDGEZERO__LIFECYCLE__VERSION, EDGEZERO__DEPLOY__TO,
# FASTLY_API_TOKEN.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

validate_inputs() {
  require_linux_x86_64
  require_input_matching fastly-service-id "${EDGEZERO__LIFECYCLE__SERVICE_ID:-}" '^[A-Za-z0-9_-]+$'
  require_input_matching fastly-version "${EDGEZERO__LIFECYCLE__VERSION:-}" '^[0-9]+$'
  require_input fastly-api-token "${FASTLY_API_TOKEN:-}"
  # A typo in deploy-to must never silently roll back production.
  case "${EDGEZERO__DEPLOY__TO:-}" in
    production | staging) ;;
    *) fail "input 'deploy-to' must be 'production' or 'staging' (got '${EDGEZERO__DEPLOY__TO:-}')" ;;
  esac
}

main() {
  validate_inputs

  local argv=("$EDGEZERO__CLI__BIN" rollback --adapter fastly --service-id "$EDGEZERO__LIFECYCLE__SERVICE_ID" --version "$EDGEZERO__LIFECYCLE__VERSION")
  if [[ "$EDGEZERO__DEPLOY__TO" == "staging" ]]; then
    argv+=(--staging)
  fi

  new_private_log
  local rc=0
  "${argv[@]}" 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?

  local rolled
  rolled=$(read_numeric_line rolled-back-to "$LIFECYCLE_LOG")
  append_output rolled-back-to "$rolled"

  if [[ "$rc" -ne 0 ]]; then
    fail "rollback failed (CLI exit $rc)"
  fi
  if [[ "$EDGEZERO__DEPLOY__TO" == "production" && -z "$rolled" ]]; then
    fail "production rollback reported success but did not emit rolled-back-to"
  fi
}

main "$@"
