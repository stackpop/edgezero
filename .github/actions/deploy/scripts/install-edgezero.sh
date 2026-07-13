#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

ACTION_ROOT=${EDGEZERO_ACTION_ROOT:?EDGEZERO_ACTION_ROOT is required}
TOOL_ROOT=${EDGEZERO_TOOL_ROOT:-${RUNNER_TEMP:-/tmp}/edgezero-action-tools}
RUST_TOOLCHAIN=${ACTION_RUST_TOOLCHAIN:?ACTION_RUST_TOOLCHAIN is required}
mkdir -p "$TOOL_ROOT"
require_cmd cargo
require_cmd rustup

rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal
cargo +"$RUST_TOOLCHAIN" install \
  --path "$ACTION_ROOT/crates/edgezero-cli" \
  --root "$TOOL_ROOT" \
  --locked \
  --force \
  --no-default-features \
  --features cli,edgezero-adapter-fastly
[[ -x "$TOOL_ROOT/bin/edgezero" ]] || fail "EdgeZero CLI installation did not produce $TOOL_ROOT/bin/edgezero"
printf '%s\n' "$TOOL_ROOT/bin" >>"${GITHUB_PATH:-/dev/null}"
export PATH="$TOOL_ROOT/bin:$PATH"
edgezero --help >/dev/null
