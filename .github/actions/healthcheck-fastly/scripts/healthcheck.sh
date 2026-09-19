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
#   EDGEZERO__LIFECYCLE__PATH             optional  URL path to probe (default: /)
#   EDGEZERO__FASTLY__API_TOKEN           staging-only  action-private provider token
#   EDGEZERO__DEPLOY__TO                  optional  production | staging (default: production)
#   EDGEZERO__LIFECYCLE__RETRY            optional  attempts before unhealthy (default: 3)
#   EDGEZERO__LIFECYCLE__RETRY_DELAY      optional  seconds between attempts (default: 5)
#   EDGEZERO__LIFECYCLE__TIMEOUT          optional  per-attempt timeout seconds (default: 10)
# Writes (outputs):
#   healthy                               true | false
#   status-code                           last HTTP status observed
# Exits non-zero when the deployment is not provably healthy (the rollback gate).

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../fastly-common/scripts/common.sh
source "$SCRIPT_DIR/../../fastly-common/scripts/common.sh"

validate_inputs() {
  require_linux_x86_64
  # `required: true` in action metadata does not fail an omitted input, so the
  # only real guard against probing with an empty service/version is this one.
  require_fastly_service_id "${EDGEZERO__LIFECYCLE__SERVICE_ID:-}"
  require_input_matching fastly-version "${EDGEZERO__LIFECYCLE__VERSION:-}" '^[0-9]+$'
  require_input_matching domain "${EDGEZERO__LIFECYCLE__DOMAIN:-}" '^[A-Za-z0-9._-]+$'
  # The path is appended to https://<domain> as one curl argument (the CLI
  # re-validates), so it must begin with '/' and carry no whitespace that would
  # break the URL or smuggle a second token.
  local probe_path="${EDGEZERO__LIFECYCLE__PATH:-/}"
  case "$probe_path" in
    /*) ;;
    *) fail "input 'path' must begin with '/' (got '$probe_path')" ;;
  esac
  [[ "$probe_path" != *[[:space:]]* ]] ||
    fail "input 'path' must not contain whitespace (got '$probe_path')"
  # `retry` is a TOTAL attempt count, so 0 is meaningless (the CLI would silently
  # clamp it to 1). Require at least one attempt rather than accept-and-coerce.
  require_input_matching retry "${EDGEZERO__LIFECYCLE__RETRY:-}" '^[1-9][0-9]*$'
  require_input_matching retry-delay "${EDGEZERO__LIFECYCLE__RETRY_DELAY:-}" '^[0-9]+$'
  # `timeout` becomes curl `--max-time`, where 0 means "no limit" — a probe could
  # then run until the whole step is externally cancelled. Require a positive value
  # (retry-delay may be 0: no wait between attempts is legitimate).
  require_input_matching timeout "${EDGEZERO__LIFECYCLE__TIMEOUT:-}" '^[1-9][0-9]*$'
  # A typo in deploy-to must never silently probe production.
  case "${EDGEZERO__DEPLOY__TO:-}" in
    production | staging) ;;
    *) fail "input 'deploy-to' must be 'production' or 'staging' (got '${EDGEZERO__DEPLOY__TO:-}')" ;;
  esac
  # The token is required ONLY for a staging probe (staging-IP resolution). A
  # production probe just curls the public domain, so it needs no token — and the
  # wrapper passes none.
  if [[ "${EDGEZERO__DEPLOY__TO:-}" == "staging" ]]; then
    require_input fastly-api-token "${EDGEZERO__FASTLY__API_TOKEN:-}"
  fi
}

main() {
  validate_inputs

  # `healthcheck` is manifest-independent (a pure API/curl probe), so it runs
  # from wherever the step is — no app-directory resolution needed.
  #
  # Resolve the CLI on its OWN line, not inside the array literal: `local argv=(
  # "$(resolve_app_cli)" … )` exits 0 even when the substitution fails (`local`
  # masks it), so a failed `:?`-guarded resolve would be swallowed and the array
  # would run with an empty argv[0]. Assign then `require_cmd` so a resolve failure
  # stops the step here (matching the other lifecycle scripts).
  local cli_bin
  cli_bin=$(resolve_app_cli)
  require_cmd "$cli_bin"
  cli_bin=$(command -v "$cli_bin") || fail "application CLI is unavailable"
  export EDGEZERO__APP__CLI__PATH="$cli_bin"
  local argv=(
    healthcheck
    --adapter fastly
    --service-id "$EDGEZERO__LIFECYCLE__SERVICE_ID"
    --version "$EDGEZERO__LIFECYCLE__VERSION"
    --domain "$EDGEZERO__LIFECYCLE__DOMAIN"
    --path "${EDGEZERO__LIFECYCLE__PATH:-/}"
    --retry "$EDGEZERO__LIFECYCLE__RETRY"
    --retry-delay "$EDGEZERO__LIFECYCLE__RETRY_DELAY"
    --timeout "$EDGEZERO__LIFECYCLE__TIMEOUT"
  )
  if [[ "$EDGEZERO__DEPLOY__TO" == "staging" ]]; then
    argv+=(--staging)
  fi

  require_cmd jq
  local workspace="${EDGEZERO__ACTION__WORKSPACE:-$(dirname -- "$cli_bin")}"
  mkdir -p "$workspace"
  export EDGEZERO__ACTION__WORKSPACE="$workspace"
  local args_file="$workspace/healthcheck-argv.nul"
  local clear_file="$workspace/fastly-provider-clear.nul"
  printf '%s\0' "${argv[@]}" >"$args_file"
  write_fastly_provider_clear_file "$clear_file"
  if [[ "$EDGEZERO__DEPLOY__TO" == "staging" ]]; then
    EDGEZERO__PROVIDER__ENV=$(jq -n --arg token "${EDGEZERO__FASTLY__API_TOKEN:-}" '{FASTLY_API_TOKEN:$token}')
  else
    EDGEZERO__PROVIDER__ENV='{}'
  fi
  export EDGEZERO__PROVIDER__ENV
  export EDGEZERO__PROVIDER__ENV_CLEAR_FILE="$clear_file"
  export EDGEZERO__APP__CLI__ARGS_FILE="$args_file"
  export EDGEZERO__APP__CLI__MUTATES=false

  new_private_log
  local rc=0
  "$SCRIPT_DIR/../../deploy-core/scripts/run-app-cli.sh" 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?

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
