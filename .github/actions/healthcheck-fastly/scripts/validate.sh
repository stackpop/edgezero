#!/usr/bin/env bash
set -euo pipefail

# Validates the healthcheck-fastly wrapper's inputs before verifying the release. In a script
# (not inline action.yml run: ) so it is linted and contract-tested.
#
# Reads (env):
#   EDGEZERO__APP__RELEASE__ARCHIVE_PRESENT required  release archive presence flag
#   EDGEZERO__APP__RELEASE__SHA256_PRESENT  required  release digest presence flag

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../fastly-common/scripts/common.sh
source "$SCRIPT_DIR/../../fastly-common/scripts/common.sh"

# The release must be pinned before its CLI can be extracted.
require_present app-release-archive "${EDGEZERO__APP__RELEASE__ARCHIVE_PRESENT:-}"
require_present app-release-sha256 "${EDGEZERO__APP__RELEASE__SHA256_PRESENT:-}"
require_fastly_service_id "${EDGEZERO__FASTLY__SERVICE_ID:-}"
