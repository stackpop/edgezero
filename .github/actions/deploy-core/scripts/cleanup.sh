#!/usr/bin/env bash
set -euo pipefail

# Removes action-owned temporary state, tool installs, and any provider auth
# state. Runs with `if: always()`, so it must tolerate partially-created dirs.

remove_if_present() {
  local dir="$1"
  # Always return success: an absent dir is not an error, and this runs under
  # `set -e` where a bare `[[…]] && rm` would exit non-zero when the dir is gone.
  if [[ -n "$dir" && -d "$dir" ]]; then
    rm -rf "$dir"
  fi
}

main() {
  remove_if_present "${EDGEZERO_ACTION_STATE_DIR:-}"
  remove_if_present "${EDGEZERO_TOOL_ROOT:-}"
  remove_if_present "${EDGEZERO_FASTLY_HOME:-}"
}

main "$@"
