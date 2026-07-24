#!/usr/bin/env bash
set -euo pipefail

# Confirms the Fastly CLI that install-fastly.sh placed in the action-owned tool
# root actually reports the pinned version. Runs after a REAL (un-faked) install
# so a version/URL/checksum bump in versions.json is validated against the
# genuine release binary — the lifecycle smoke test fakes the CLI and never
# exercises this.
#
# Reads (env):
#   EDGEZERO__FASTLY__VERSIONS_JSON   required  pinned metadata (holds fastly.version)
#   EDGEZERO__ACTION__TOOL_ROOT       required  the install dir install-fastly.sh wrote to

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

main() {
  local versions_json="${EDGEZERO__FASTLY__VERSIONS_JSON:?EDGEZERO__FASTLY__VERSIONS_JSON is required}"
  local tool_root="${EDGEZERO__ACTION__TOOL_ROOT:?EDGEZERO__ACTION__TOOL_ROOT is required}"

  require_cmd jq
  local pinned fastly_bin
  pinned=$(json_get "$versions_json" fastly.version)
  fastly_bin="$tool_root/provider-bin/fastly"

  [[ -x "$fastly_bin" ]] ||
    fail "installer did not place an executable fastly at $fastly_bin"

  # Capture the full output and take the first line via parameter expansion —
  # piping to `head` would SIGPIPE `fastly` (exit 141) under `set -o pipefail`.
  local reported
  reported=$("$fastly_bin" version)
  reported=${reported%%$'\n'*}

  [[ "$reported" == *"$pinned"* ]] ||
    fail "installed Fastly CLI reports '$reported', expected version $pinned"

  notice "installed Fastly CLI reports the pinned version $pinned: $reported"
}

main "$@"
