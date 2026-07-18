#!/usr/bin/env bash
set -euo pipefail

# Validates the config-push-fastly wrapper's inputs. In a script (not inline
# action.yml `run:`) so it is shellcheck'd and contract-tested.
#
# Reads (env):
#   EDGEZERO__APP__CLI__ARTIFACT_PRESENT  required  "true" when app-cli-artifact is non-empty
#   EDGEZERO__FASTLY__API_TOKEN_PRESENT   required  "true" when fastly-api-token is non-empty
#   EDGEZERO__DEPLOY__TO                  optional  production | staging (default: production)
#   EDGEZERO__CONFIG_PUSH__KEY_PRESENT    optional  "true" when an explicit key was supplied

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

main() {
  require_present app-cli-artifact "${EDGEZERO__APP__CLI__ARTIFACT_PRESENT:-}"
  require_present fastly-api-token "${EDGEZERO__FASTLY__API_TOKEN_PRESENT:-}"
  local deploy_to="${EDGEZERO__DEPLOY__TO:-production}"
  # A typo in deploy-to must never silently push to production.
  case "$deploy_to" in
    production | staging) ;;
    *) fail "input 'deploy-to' must be 'production' or 'staging' (got '${EDGEZERO__DEPLOY__TO:-}')" ;;
  esac
  # A staging push derives its key from the store's logical id (`<id>_staging`),
  # which is what the staging selector store points a staged version at. An
  # explicit `key` would be written to a key nothing reads, so the CLI refuses
  # the combination — reject it here with a clearer, earlier message.
  if [[ "$deploy_to" == "staging" && "${EDGEZERO__CONFIG_PUSH__KEY_PRESENT:-}" == "true" ]]; then
    fail "input 'key' cannot be combined with deploy-to: staging; the staging key is derived from the store's logical id (<id>_staging). Push to production with 'key', or push staging without it."
  fi
}

main "$@"
