#!/usr/bin/env bash
set -euo pipefail

# Validates the config-push-fastly wrapper's inputs. In a script (not inline
# action.yml `run:`) so it is shellcheck'd and contract-tested.
#
# Reads (env):
#   EDGEZERO__APP__CLI__ARTIFACT_PRESENT  required  "true" when app-cli-artifact is non-empty
#   EDGEZERO__FASTLY__API_TOKEN_PRESENT   required  "true" when fastly-api-token is non-empty
#   EDGEZERO__DEPLOY__TO                  optional  production | staging (default: production)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

main() {
  require_present app-cli-artifact "${EDGEZERO__APP__CLI__ARTIFACT_PRESENT:-}"
  require_present fastly-api-token "${EDGEZERO__FASTLY__API_TOKEN_PRESENT:-}"
  # A typo in deploy-to must never silently push to production.
  case "${EDGEZERO__DEPLOY__TO:-production}" in
    production | staging) ;;
    *) fail "input 'deploy-to' must be 'production' or 'staging' (got '${EDGEZERO__DEPLOY__TO:-}')" ;;
  esac
}

main "$@"
