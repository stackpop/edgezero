#!/usr/bin/env bash
set -euo pipefail

ROOT=${EDGEZERO_ACTION_STATE_DIR:-}
if [[ -n "$ROOT" && -d "$ROOT" ]]; then
  rm -rf "$ROOT"
fi

TOOL_ROOT=${EDGEZERO_TOOL_ROOT:-}
if [[ -n "$TOOL_ROOT" && -d "$TOOL_ROOT" ]]; then
  rm -rf "$TOOL_ROOT"
fi

# Remove action-owned Fastly auth state if future installers create it.
if [[ -n "${EDGEZERO_FASTLY_HOME:-}" && -d "$EDGEZERO_FASTLY_HOME" ]]; then
  rm -rf "$EDGEZERO_FASTLY_HOME"
fi
