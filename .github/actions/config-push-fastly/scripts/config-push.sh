#!/usr/bin/env bash
set -euo pipefail

# Pushes the application's typed config to a Fastly config store, and emits the
# key that was written.
#
# Like healthcheck.sh and rollback.sh (its sibling lifecycle actions), this calls
# the app CLI directly with FASTLY_API_TOKEN in the step env — the adapter's own
# convention, which `fastly config-store-entry update` reads to authenticate. The
# wrapper blanks every other FASTLY_* alias, so an inherited FASTLY_ENDPOINT or
# FASTLY_TOKEN can never redirect or re-auth the push.
#
# Staging (spec §5.5): `deploy-to: staging` passes `--staging` to the CLI, which
# writes the `<key>_staging` variant in the SAME store — never the production key
# the live service reads.
#
# Reads (env):
#   EDGEZERO__APP__CLI__BIN               required  app CLI binary to invoke
#   FASTLY_API_TOKEN                      required  provider token (Fastly's own convention)
#   EDGEZERO__PROJECT__WORKING_DIRECTORY  required  directory to run the CLI from
#   EDGEZERO__DEPLOY__TO                  optional  production | staging (default: production)
#   EDGEZERO__CONFIG_PUSH__STORE          optional  logical config-store id
#   EDGEZERO__CONFIG_PUSH__KEY            optional  explicit base key
#   EDGEZERO__CONFIG_PUSH__MANIFEST       optional  edgezero.toml path (relative to the app dir)
#   EDGEZERO__CONFIG_PUSH__APP_CONFIG     optional  typed config file path
# Writes (outputs):
#   pushed-key                            the key written (base, or its _staging variant)
#   store                                 the logical store id, when supplied

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

main() {
  local cli_bin="${EDGEZERO__APP__CLI__BIN:?EDGEZERO__APP__CLI__BIN is required}"
  local working_directory="${EDGEZERO__PROJECT__WORKING_DIRECTORY:?EDGEZERO__PROJECT__WORKING_DIRECTORY is required}"
  local deploy_to="${EDGEZERO__DEPLOY__TO:-production}"
  local store="${EDGEZERO__CONFIG_PUSH__STORE:-}"
  local key="${EDGEZERO__CONFIG_PUSH__KEY:-}"
  local manifest="${EDGEZERO__CONFIG_PUSH__MANIFEST:-}"
  local app_config="${EDGEZERO__CONFIG_PUSH__APP_CONFIG:-}"

  require_input fastly-api-token "${FASTLY_API_TOKEN:-}"
  require_cmd "$cli_bin"
  # A typo in deploy-to must never silently push to production.
  case "$deploy_to" in
    production | staging) ;;
    *) fail "input 'deploy-to' must be 'production' or 'staging' (got '$deploy_to')" ;;
  esac

  # Build the argv through a Bash array — never eval. --yes and --no-diff make the
  # push non-interactive in CI; --staging selects the `<key>_staging` variant.
  local argv=("$cli_bin" config push --adapter fastly)
  if [[ -n "$manifest" ]]; then argv+=(--manifest "$manifest"); fi
  if [[ -n "$app_config" ]]; then argv+=(--app-config "$app_config"); fi
  if [[ -n "$store" ]]; then argv+=(--store "$store"); fi
  if [[ -n "$key" ]]; then argv+=(--key "$key"); fi
  if [[ "$deploy_to" == "staging" ]]; then argv+=(--staging); fi
  argv+=(--yes --no-diff)

  new_private_log
  local rc=0
  (cd "$working_directory" && "${argv[@]}") 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    fail "config push failed (CLI exit $rc)"
  fi

  # Anchored parse of the canonical `pushed-key=<key>` line the CLI emits.
  local pushed
  pushed=$(grep -oE '^pushed-key=[A-Za-z0-9._-]+$' "$LIFECYCLE_LOG" | tail -n 1 | cut -d= -f2 || true)
  [[ -n "$pushed" ]] ||
    fail "config push reported success but emitted no canonical 'pushed-key=<key>' line"

  append_output pushed-key "$pushed"
  append_output store "$store"
}

main "$@"
