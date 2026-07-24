#!/usr/bin/env bash
set -euo pipefail

# Runs the application CLI's deploy through the provider-env credential boundary
# and emits the resulting Fastly version.
#
# Credentials are handed to run-app-cli.sh as DATA (a JSON object), not as FASTLY_*
# aliases on this step. run-app-cli.sh clears every declared alias — including any
# inherited FASTLY_ENDPOINT / FASTLY_TOKEN — exports only these typed values, and
# then scrubs its own private variables (including this JSON) before exec'ing the
# CLI. Building the JSON here, from step `env:`, is also what keeps the secret out
# of an interpolated `run:` block.
#
# Reads (env):
#   EDGEZERO__FASTLY__API_TOKEN           required  typed Fastly API token
#   EDGEZERO__FASTLY__SERVICE_ID          required  typed Fastly service id
#   (plus the run-app-cli.sh Reads contract, which this delegates to)
# Writes (outputs):
#   fastly-version                        the deployed/staged Fastly version

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

main() {
  local token="${EDGEZERO__FASTLY__API_TOKEN:-}"
  local service_id="${EDGEZERO__FASTLY__SERVICE_ID:-}"

  require_input fastly-api-token "$token"
  require_input_matching fastly-service-id "$service_id" '^[A-Za-z0-9_-]+$'
  require_cmd jq

  EDGEZERO__PROVIDER__ENV=$(jq -n --arg t "$token" --arg s "$service_id" \
    '{FASTLY_API_TOKEN: $t, FASTLY_SERVICE_ID: $s}')
  export EDGEZERO__PROVIDER__ENV

  new_private_log
  "$SCRIPT_DIR/../../deploy-core/scripts/run-app-cli.sh" deploy 2>&1 | tee "$LIFECYCLE_LOG"

  local version
  version=$(read_numeric_line version "$LIFECYCLE_LOG")
  [[ -n "$version" ]] ||
    fail "deploy reported success but emitted no canonical 'version=<digits>' line, so there is no version to thread into healthcheck or rollback"

  append_output fastly-version "$version"
}

main "$@"
