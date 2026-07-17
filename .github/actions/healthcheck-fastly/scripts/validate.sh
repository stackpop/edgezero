#!/usr/bin/env bash
set -euo pipefail

# Validates the healthcheck-fastly wrapper's inputs before downloading the artifact. In a script
# (not inline action.yml run: ) so it is linted and contract-tested.
#
# Reads (env):
#   EDGEZERO__APP__CLI__ARTIFACT_PRESENT  required  "true" when app-cli-artifact is non-empty

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

# An empty artifact name makes actions/download-artifact fetch EVERY artifact in
# the run, so the CLI we then execute would be arbitrary.
require_present app-cli-artifact "${EDGEZERO__APP__CLI__ARTIFACT_PRESENT:-}"
