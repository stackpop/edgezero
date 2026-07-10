#!/usr/bin/env bash
set -euo pipefail

# Extracts the build-cli artifact tar (downloaded by actions/download-artifact)
# into an action-owned tool dir, preserving the executable bit, reads the
# self-describing cli-meta.json, and prepends the dir to PATH for action steps.
# A wrapper-supplied CLI_BIN overrides the metadata's binary name.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

ARTIFACT_DIR=${EDGEZERO_CLI_ARTIFACT_DIR:?EDGEZERO_CLI_ARTIFACT_DIR is required}
CLI_BIN_OVERRIDE=${INPUT_CLI_BIN:-}
TOOL_ROOT=${EDGEZERO_TOOL_ROOT:-${RUNNER_TEMP:-/tmp}/edgezero-action-tools}
require_cmd jq
require_cmd tar

mkdir -p "$TOOL_ROOT/bin"

# The artifact contains a single tar (built by build-cli). Locate it.
TARBALL=$(find "$ARTIFACT_DIR" -maxdepth 2 -type f -name '*.tar' | head -n 1)
[[ -n "$TARBALL" ]] || fail "no CLI tar found under the downloaded artifact at '$ARTIFACT_DIR'"

tar -xf "$TARBALL" -C "$TOOL_ROOT/bin"
[[ -f "$TOOL_ROOT/bin/cli-meta.json" ]] || fail "CLI artifact is missing cli-meta.json"

META_BIN=$(jq -er '."cli-bin"' "$TOOL_ROOT/bin/cli-meta.json") || fail "cli-meta.json has no cli-bin"
CLI_VERSION=$(jq -er '."cli-version"' "$TOOL_ROOT/bin/cli-meta.json") || fail "cli-meta.json has no cli-version"
CLI_BIN=${CLI_BIN_OVERRIDE:-$META_BIN}

[[ -f "$TOOL_ROOT/bin/$CLI_BIN" ]] || fail "CLI binary '$CLI_BIN' not present in the artifact"
chmod +x "$TOOL_ROOT/bin/$CLI_BIN"
"$TOOL_ROOT/bin/$CLI_BIN" --help >/dev/null 2>&1 || fail "downloaded CLI '$CLI_BIN' did not run '--help'"

printf '%s\n' "$TOOL_ROOT/bin" >>"${GITHUB_PATH:-/dev/null}"
export PATH="$TOOL_ROOT/bin:$PATH"

notice "using app CLI '$CLI_BIN' v$CLI_VERSION from artifact"
append_output cli-bin "$CLI_BIN"
append_output cli-version "$CLI_VERSION"
append_env EDGEZERO_TOOL_ROOT "$TOOL_ROOT"
