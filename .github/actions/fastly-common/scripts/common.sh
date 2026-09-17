#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

require_fastly_service_id() {
  require_input_matching fastly-service-id "$1" '^[A-Za-z0-9]+$'
}
