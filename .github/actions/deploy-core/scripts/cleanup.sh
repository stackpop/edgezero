#!/usr/bin/env bash
set -euo pipefail

# Removes action-owned temporary state, tool installs, and any provider auth
# state. Runs with `if: always()`, so it must tolerate partially-created dirs.

remove_if_present() {
  local dir="$1"
  [[ -n "$dir" && -d "$dir" ]] && rm -rf "$dir"
}

main() {
  remove_if_present "${EDGEZERO_ACTION_STATE_DIR:-}"
  remove_if_present "${EDGEZERO_TOOL_ROOT:-}"
  remove_if_present "${EDGEZERO_FASTLY_HOME:-}"
}

main "$@"
