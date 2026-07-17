#!/usr/bin/env bash
set -euo pipefail

# Validates the deploy-fastly wrapper's own (Fastly-specific) inputs, then
# delegates the provider-neutral checks to the engine's validate-inputs.sh.
#
# It is a script rather than inline action.yml, so CI lints and contract-tests it
# like every other action script. The Fastly-specific checks stay in the WRAPPER,
# never in the provider-neutral engine (which hard-codes no provider names).
#
# The credential is checked by PRESENCE, not value: the token never reaches this
# step (it is scoped to the deploy step only), so the wrapper passes a
# precomputed `…_PRESENT` boolean instead.
#
# Reads (env):
#   EDGEZERO__APP__CLI__ARTIFACT_PRESENT  required  "true" when app-cli-artifact is non-empty
#   EDGEZERO__FASTLY__API_TOKEN_PRESENT   required  "true" when fastly-api-token is non-empty
#   EDGEZERO__FASTLY__SERVICE_ID          required  the Fastly service id
#   (plus the validate-inputs.sh Reads contract, which this delegates to)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

main() {
  # GitHub does not enforce `required: true` on composite inputs. An empty
  # artifact name makes actions/download-artifact fetch EVERY artifact in the
  # run, so the CLI we then execute with credentials would be arbitrary.
  require_present app-cli-artifact "${EDGEZERO__APP__CLI__ARTIFACT_PRESENT:-}"
  require_present fastly-api-token "${EDGEZERO__FASTLY__API_TOKEN_PRESENT:-}"
  require_input_matching fastly-service-id "${EDGEZERO__FASTLY__SERVICE_ID:-}" '^[A-Za-z0-9_-]+$'

  # Provider-neutral validation (adapter, booleans, JSON-array args, the
  # allowlist). It also rejects a non-boolean 'stage' before any deploy.
  "$SCRIPT_DIR/../../deploy-core/scripts/validate-inputs.sh"
}

main "$@"
