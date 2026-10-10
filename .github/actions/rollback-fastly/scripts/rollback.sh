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
#   EDGEZERO__FASTLY__API_TOKEN           required  action-private Fastly API token
#   EDGEZERO__DEPLOY__TO                  optional  production | staging (default: production)
# Writes (outputs):
#   mutation-attempted                    true, emitted before the CLI runs (reconcile signal)
#   rolled-back-to                        the activated version (production only)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../fastly-common/scripts/common.sh
source "$SCRIPT_DIR/../../fastly-common/scripts/common.sh"

validate_inputs() {
  require_linux_x86_64
  require_fastly_service_id "${EDGEZERO__LIFECYCLE__SERVICE_ID:-}"
  require_input_matching fastly-version "${EDGEZERO__LIFECYCLE__VERSION:-}" '^[0-9]+$'
  require_input fastly-api-token "${EDGEZERO__FASTLY__API_TOKEN:-}"
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

  # `rollback` is manifest-independent (a pure Fastly-API call), so it runs from
  # wherever the step is — no app-directory resolution needed. Resolve and VERIFY
  # the CLI before signalling a mutation: a missing binary must not falsely claim a
  # rollback was attempted.
  local cli_bin
  cli_bin=$(resolve_app_cli)
  require_cmd "$cli_bin"
  cli_bin=$(command -v "$cli_bin") || fail "application CLI is unavailable"
  export EDGEZERO__APP__CLI__PATH="$cli_bin"
  local argv=(rollback --adapter fastly --service-id "$EDGEZERO__LIFECYCLE__SERVICE_ID" --version "$EDGEZERO__LIFECYCLE__VERSION")
  if [[ "$EDGEZERO__DEPLOY__TO" == "staging" ]]; then
    argv+=(--staging)
  else
    argv+=(--rollback-to "$EDGEZERO__LIFECYCLE__ROLLBACK_TO")
  fi

  require_cmd jq
  local workspace="${EDGEZERO__ACTION__WORKSPACE:-$(dirname -- "$cli_bin")}"
  mkdir -p "$workspace"
  export EDGEZERO__ACTION__WORKSPACE="$workspace"
  local args_file="$workspace/rollback-argv.nul"
  local clear_file="$workspace/fastly-provider-clear.nul"
  printf '%s\0' "${argv[@]}" >"$args_file"
  write_fastly_provider_clear_file "$clear_file"
  EDGEZERO__PROVIDER__ENV=$(jq -n --arg token "${EDGEZERO__FASTLY__API_TOKEN:-}" '{FASTLY_API_TOKEN:$token}')
  export EDGEZERO__PROVIDER__ENV
  export EDGEZERO__PROVIDER__ENV_CLEAR_FILE="$clear_file"
  export EDGEZERO__APP__CLI__ARGS_FILE="$args_file"
  export EDGEZERO__APP__CLI__MUTATES=true

  new_private_log
  local rc=0
  "$SCRIPT_DIR/../../deploy-core/scripts/run-app-cli.sh" 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?

  # Surface the CLI's exit status BEFORE writing any output, so an output-write
  # failure can never replace the real provider result.
  if [[ "$rc" -ne 0 ]]; then
    fail_with "$rc" "rollback failed (CLI exit $rc)"
  fi

  local rolled
  rolled=$(read_numeric_line rolled-back-to "$LIFECYCLE_LOG")
  append_output rolled-back-to "$rolled"

  if [[ "$EDGEZERO__DEPLOY__TO" == "production" && -z "$rolled" ]]; then
    fail "production rollback reported success but did not emit rolled-back-to"
  fi
}

main "$@"
