#!/usr/bin/env bash
set -euo pipefail

# Validates the config-push-fastly wrapper's inputs. In a script (not inline
# action.yml `run:`) so it is shellcheck'd and contract-tested.
#
# Reads (env):
#   EDGEZERO__APP__RELEASE__ARCHIVE_PRESENT required  release archive presence flag
#   EDGEZERO__APP__RELEASE__SHA256_PRESENT  required  release digest presence flag
#   EDGEZERO__FASTLY__API_TOKEN_PRESENT   required  "true" when fastly-api-token is non-empty
#   EDGEZERO__DEPLOY__TO                  optional  production | staging (default: production)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

main() {
  require_present app-release-archive "${EDGEZERO__APP__RELEASE__ARCHIVE_PRESENT:-}"
  require_present app-release-sha256 "${EDGEZERO__APP__RELEASE__SHA256_PRESENT:-}"
  require_present fastly-api-token "${EDGEZERO__FASTLY__API_TOKEN_PRESENT:-}"
  local has_file="${EDGEZERO__CONFIG_PUSH__APP_CONFIG_PRESENT:-false}"
  local has_inline="${EDGEZERO__CONFIG_PUSH__APP_CONFIG_INLINE_PRESENT:-false}"
  if [[ "$has_file" == "$has_inline" ]]; then
    fail "exactly one of 'app-config' or 'app-config-inline' is required"
  fi
  local deploy_to="${EDGEZERO__DEPLOY__TO:-production}"
  # A typo in deploy-to must never silently push to production.
  case "$deploy_to" in
    production | staging) ;;
    *) fail "input 'deploy-to' must be 'production' or 'staging' (got '${EDGEZERO__DEPLOY__TO:-}')" ;;
  esac
}

main "$@"
