#!/usr/bin/env bash
set -euo pipefail

# Probes a deployed Fastly version through the application CLI and fails closed.
#
# Callers gate their rollback on this action FAILING. So every path that cannot
# prove the deployment is healthy must exit non-zero: a non-zero CLI, a
# `healthy=false` verdict, and — critically — no verdict at all.
#
# Reads (env):
#   EDGEZERO__APP__CLI__PATH              optional  absolute path to the app CLI (preferred; avoids PATH shadowing)
#   EDGEZERO__APP__CLI__BIN               optional  app CLI name, used when __PATH is unset
#   EDGEZERO__LIFECYCLE__SERVICE_ID       required  Fastly service id
#   EDGEZERO__LIFECYCLE__VERSION          required  version to probe
#   EDGEZERO__LIFECYCLE__DOMAIN           required  domain to probe
#   FASTLY_API_TOKEN                      staging-only  provider token (staging-IP resolution)
#   EDGEZERO__DEPLOY__TO                  optional  production | staging (default: production)
#   EDGEZERO__LIFECYCLE__RETRY            optional  attempts before unhealthy (default: 3)
#   EDGEZERO__LIFECYCLE__RETRY_DELAY      optional  seconds between attempts (default: 5)
#   EDGEZERO__LIFECYCLE__TIMEOUT          optional  per-attempt timeout seconds (default: 10)
# Writes (outputs):
#   healthy                               true | false
#   status-code                           last HTTP status observed
# Exits non-zero when the deployment is not provably healthy (the rollback gate).

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

validate_inputs() {
  require_linux_x86_64
  # `required: true` in action metadata does not fail an omitted input, so the
  # only real guard against probing with an empty service/version is this one.
  require_input_matching fastly-service-id "${EDGEZERO__LIFECYCLE__SERVICE_ID:-}" '^[A-Za-z0-9_-]+$'
  require_input_matching fastly-version "${EDGEZERO__LIFECYCLE__VERSION:-}" '^[0-9]+$'
  require_input_matching domain "${EDGEZERO__LIFECYCLE__DOMAIN:-}" '^[A-Za-z0-9._-]+$'
  # `retry` is a TOTAL attempt count, so 0 is meaningless (the CLI would silently
  # clamp it to 1). Require at least one attempt rather than accept-and-coerce.
  require_input_matching retry "${EDGEZERO__LIFECYCLE__RETRY:-}" '^[1-9][0-9]*$'
  require_input_matching retry-delay "${EDGEZERO__LIFECYCLE__RETRY_DELAY:-}" '^[0-9]+$'
  require_input_matching timeout "${EDGEZERO__LIFECYCLE__TIMEOUT:-}" '^[0-9]+$'
  # A typo in deploy-to must never silently probe production.
  case "${EDGEZERO__DEPLOY__TO:-}" in
    production | staging) ;;
    *) fail "input 'deploy-to' must be 'production' or 'staging' (got '${EDGEZERO__DEPLOY__TO:-}')" ;;
  esac
  # The token is required ONLY for a staging probe (staging-IP resolution). A
  # production probe just curls the public domain, so it needs no token — and the
  # wrapper passes none.
  if [[ "${EDGEZERO__DEPLOY__TO:-}" == "staging" ]]; then
    require_input fastly-api-token "${FASTLY_API_TOKEN:-}"
  fi
}

main() {
  validate_inputs

  # `healthcheck` is manifest-independent (a pure API/curl probe), so it runs
  # from wherever the step is — no app-directory resolution needed.
  local argv=(
    "$(resolve_app_cli)" healthcheck
    --adapter fastly
    --service-id "$EDGEZERO__LIFECYCLE__SERVICE_ID"
    --version "$EDGEZERO__LIFECYCLE__VERSION"
    --domain "$EDGEZERO__LIFECYCLE__DOMAIN"
    --retry "$EDGEZERO__LIFECYCLE__RETRY"
    --retry-delay "$EDGEZERO__LIFECYCLE__RETRY_DELAY"
    --timeout "$EDGEZERO__LIFECYCLE__TIMEOUT"
  )
  if [[ "$EDGEZERO__DEPLOY__TO" == "staging" ]]; then
    argv+=(--staging)
  fi

  new_private_log
  local rc=0
  "${argv[@]}" 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?

  local healthy status
  healthy=$(read_bool_line healthy "$LIFECYCLE_LOG")
  status=$(read_numeric_line status-code "$LIFECYCLE_LOG")
  append_output healthy "${healthy:-false}"
  append_output status-code "$status"

  if [[ "$rc" -ne 0 ]]; then
    fail_with "$rc" "health check failed (CLI exit $rc, healthy=${healthy:-<none>}, status=${status:-<none>})"
  fi
  if [[ "$healthy" != "true" ]]; then
    fail "health check did not report healthy=true (got '${healthy:-<no verdict emitted>}')"
  fi
}

main "$@"
