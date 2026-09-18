#!/usr/bin/env bash
set -euo pipefail

# Shared Fastly policy for lifecycle action wrappers. Provider-neutral action
# cores source only deploy-core/common.sh and never this file.
#
# Defines:
#   Fastly service-ID validation
#   the complete Fastly environment-alias clear list
#   Fastly-only public runtime environment names
#
# Reads/Writes:
#   no environment values or outputs at source time; helper arguments identify
#   action-owned files written as NUL-delimited name lists.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

require_fastly_service_id() {
  require_input_matching fastly-service-id "$1" '^[A-Za-z0-9]+$'
}

write_fastly_provider_clear_file() {
  local file="$1"
  printf '%s\0' \
    FASTLY_API_TOKEN FASTLY_SERVICE_ID FASTLY_TOKEN FASTLY_KEY FASTLY_API_KEY \
    FASTLY_AUTH_TOKEN FASTLY_API_ENDPOINT FASTLY_ENDPOINT FASTLY_API_URL \
    FASTLY_PROFILE FASTLY_SERVICE_NAME FASTLY_DEBUG FASTLY_DEBUG_MODE \
    FASTLY_CONFIG_FILE FASTLY_CARGO_PROFILE FASTLY_HOME >"$file"
}

write_fastly_public_runtime_file() {
  local file="$1"
  printf '%s\0' EDGEZERO__LOGGING__USE_FASTLY_LOGGER EDGEZERO__LOGGING__ECHO_STDOUT >"$file"
}
