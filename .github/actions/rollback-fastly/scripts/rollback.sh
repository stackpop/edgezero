#!/usr/bin/env bash
set -euo pipefail

# Rolls a Fastly deployment back through the application CLI.
#
# Production activates the previous version; staging deactivates the staged one.
# Fails closed: a rollback that cannot say what it activated has not provably
# rolled anything back.
#
# Reads (env):
#   EDGEZERO__APP__CLI__PATH              optional  absolute path to the app CLI (preferred; avoids PATH shadowing)
#   EDGEZERO__APP__CLI__BIN               optional  app CLI name, used when __PATH is unset
#   EDGEZERO__LIFECYCLE__SERVICE_ID       required  Fastly service id
#   EDGEZERO__LIFECYCLE__VERSION          required  the current (bad) version to roll back from
#   EDGEZERO__LIFECYCLE__ROLLBACK_TO      required (production)  the version to re-activate
#   FASTLY_API_TOKEN                      required  provider token (Fastly's own convention)
#   EDGEZERO__DEPLOY__TO                  optional  production | staging (default: production)
# Writes (outputs):
#   rolled-back-to                        the activated version (production only)

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
    production)
      # Fastly cannot infer the previously-live version, so production requires
      # an explicit target (wired from deploy-fastly's previous-version output).
      require_input_matching rollback-to "${EDGEZERO__LIFECYCLE__ROLLBACK_TO:-}" '^[0-9]+$'
      ;;
    staging) ;;
    *) fail "input 'deploy-to' must be 'production' or 'staging' (got '${EDGEZERO__DEPLOY__TO:-}')" ;;
  esac
}

main() {
  validate_inputs

  local cli_bin
  cli_bin=$(resolve_app_cli)
  # Absolutize a relative CLI path before cd, then run from the app dir so the
  # CLI's manifest load is correct in a monorepo.
  case "$cli_bin" in /*) ;; *) [[ -e "$cli_bin" ]] && cli_bin=$(canonical_path "$cli_bin") ;; esac
  enter_app_dir "${EDGEZERO__PROJECT__WORKING_DIRECTORY:-.}"

  local argv=("$cli_bin" rollback --adapter fastly --service-id "$EDGEZERO__LIFECYCLE__SERVICE_ID" --version "$EDGEZERO__LIFECYCLE__VERSION")
  if [[ "$EDGEZERO__DEPLOY__TO" == "staging" ]]; then
    argv+=(--staging)
  else
    argv+=(--rollback-to "$EDGEZERO__LIFECYCLE__ROLLBACK_TO")
  fi

  new_private_log
  local rc=0
  "${argv[@]}" 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?

  local rolled
  rolled=$(read_numeric_line rolled-back-to "$LIFECYCLE_LOG")
  append_output rolled-back-to "$rolled"

  if [[ "$rc" -ne 0 ]]; then
    fail_with "$rc" "rollback failed (CLI exit $rc)"
  fi
  if [[ "$EDGEZERO__DEPLOY__TO" == "production" && -z "$rolled" ]]; then
    fail "production rollback reported success but did not emit rolled-back-to"
  fi
}

main "$@"
