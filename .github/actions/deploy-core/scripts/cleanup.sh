#!/usr/bin/env bash
set -euo pipefail

# Removes action-owned temporary state and tool installs. Runs with
# `if: always()`, so it must tolerate partially-created dirs.
#
# This script does `rm -rf`, so it removes ONLY directories it can prove the
# action owns: real paths strictly beneath RUNNER_TEMP. An inherited or
# job-level value pointing at the checkout — or anywhere else on a self-hosted
# runner — is refused, not deleted. (An earlier revision removed
# `$EDGEZERO_FASTLY_HOME`, a variable nothing in the action ever set: its value
# could only ever come from the caller's environment.)
#
# Reads (env):
#   RUNNER_TEMP                           required  the only root anything may be removed beneath
#   EDGEZERO__ACTION__STATE_DIR           optional  action-owned state dir to remove
#   EDGEZERO__ACTION__TOOL_ROOT           optional  action-owned tool install to remove
# Writes:
#   nothing — removes action-owned dirs; emits no outputs.

remove_owned_dir() {
  local dir="$1" temp_root="$2"

  [[ -n "$dir" ]] || return 0
  [[ -d "$dir" ]] || return 0

  # Resolve symlinks before comparing: a symlinked state dir must not be able to
  # smuggle the removal outside the temp root.
  local real_dir real_root
  real_dir=$(cd -- "$dir" 2>/dev/null && pwd -P) || return 0
  real_root=$(cd -- "$temp_root" 2>/dev/null && pwd -P) || {
    echo "[edgezero-action] cleanup: RUNNER_TEMP '$temp_root' does not exist; nothing removed" >&2
    return 0
  }

  if [[ "$real_dir" != "$real_root"/* ]]; then
    echo "[edgezero-action] cleanup: refusing to remove '$dir': not beneath the action-owned temp root '$real_root'" >&2
    return 0
  fi

  rm -rf "$real_dir"
}

main() {
  local temp_root="${RUNNER_TEMP:-}"
  if [[ -z "$temp_root" ]]; then
    echo "[edgezero-action] cleanup: RUNNER_TEMP is unset; nothing removed" >&2
    return 0
  fi

  remove_owned_dir "${EDGEZERO__ACTION__STATE_DIR:-}" "$temp_root"
  remove_owned_dir "${EDGEZERO__ACTION__TOOL_ROOT:-}" "$temp_root"
}

main "$@"
