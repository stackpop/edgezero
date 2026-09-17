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
#   EDGEZERO__APP__RELEASE__ARCHIVE_PRESENT required  release archive presence flag
#   EDGEZERO__APP__RELEASE__SHA256_PRESENT  required  release digest presence flag
#   EDGEZERO__FASTLY__API_TOKEN_PRESENT   required  "true" when fastly-api-token is non-empty
#   EDGEZERO__FASTLY__SERVICE_ID          required  the Fastly service id
#   (plus the validate-inputs.sh Reads contract, which this delegates to)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../fastly-common/scripts/common.sh
source "$SCRIPT_DIR/../../fastly-common/scripts/common.sh"

main() {
  # GitHub does not enforce `required: true` on composite inputs. Require both
  # release coordinates before any application CLI or provider command runs.
  require_present app-release-archive "${EDGEZERO__APP__RELEASE__ARCHIVE_PRESENT:-}"
  require_present app-release-sha256 "${EDGEZERO__APP__RELEASE__SHA256_PRESENT:-}"
  require_present fastly-api-token "${EDGEZERO__FASTLY__API_TOKEN_PRESENT:-}"
  require_fastly_service_id "${EDGEZERO__FASTLY__SERVICE_ID:-}"

  # Provider-neutral validation (adapter, booleans, JSON-array args, the
  # allowlist). It also rejects a 'deploy-to' that is neither production nor
  # staging before any deploy (a typo must never silently reach production).
  "$SCRIPT_DIR/../../deploy-core/scripts/validate-inputs.sh"
}

main "$@"
