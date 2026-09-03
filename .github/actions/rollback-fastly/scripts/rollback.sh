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
#   mutation-attempted                    true, emitted before the CLI runs (reconcile signal)
#   rolled-back-to                        the activated version (production only)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

validate_inputs() {
  require_linux_x86_64
  require_input_matching fastly-service-id "${EDGEZERO__LIFECYCLE__SERVICE_ID:-}" '^[A-Za-z0-9]+$'
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

  # `rollback` is manifest-independent (a pure Fastly-API call), so it runs from
  # wherever the step is — no app-directory resolution needed. Resolve and VERIFY
  # the CLI before signalling a mutation: a missing binary must not falsely claim a
  # rollback was attempted.
  local cli_bin
  cli_bin=$(resolve_app_cli)
  require_cmd "$cli_bin"
  local argv=("$cli_bin" rollback --adapter fastly --service-id "$EDGEZERO__LIFECYCLE__SERVICE_ID" --version "$EDGEZERO__LIFECYCLE__VERSION")
  if [[ "$EDGEZERO__DEPLOY__TO" == "staging" ]]; then
    argv+=(--staging)
  else
    argv+=(--rollback-to "$EDGEZERO__LIFECYCLE__ROLLBACK_TO")
  fi

  new_private_log
  # Record that a provider mutation is being ATTEMPTED after setup (CLI verified)
  # and immediately before the CLI runs: a setup failure never falsely signals, and
  # because it lands in GITHUB_OUTPUT before the mutation starts it CAN survive a
  # cancel/timeout mid-activation (best-effort — a hard runner loss can still drop
  # it, so its absence is not proof of no mutation; read via `if: always()`).
  append_output mutation-attempted true
  local rc=0
  "${argv[@]}" 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?

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
