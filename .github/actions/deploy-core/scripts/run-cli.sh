#!/usr/bin/env bash
set -euo pipefail

# Runs the application CLI (build or deploy) through Bash arrays — never eval.
# Provider-neutral: it invokes `<cli-bin>` with the adapter, the wrapper's typed
# deploy-flags (before `--`), and caller passthrough deploy-args (after `--`).
# Credential scoping is done by the wrapper via step-level env: — this script
# only clears the wrapper-named aliases during a credential-free build.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

MODE=${1:?usage: run-cli.sh build|deploy}
case "$MODE" in build | deploy) ;; *) fail "mode must be build or deploy" ;; esac

CLI_BIN=${EDGEZERO_CLI_BIN:?EDGEZERO_CLI_BIN is required}
ADAPTER=${EDGEZERO_ADAPTER:?EDGEZERO_ADAPTER is required}
WORKING_DIRECTORY=${EDGEZERO_WORKING_DIRECTORY:?EDGEZERO_WORKING_DIRECTORY is required}
MANIFEST=${EDGEZERO_MANIFEST_PATH:-}
require_cmd "$CLI_BIN"

read_nul_into() {
  # read_nul_into <array-name> <file>
  local -n _dest="$1"
  local file="$2"
  [[ -s "$file" ]] || return 0
  local item
  while IFS= read -r -d '' item; do
    _dest+=("$item")
  done <"$file"
}

clear_named_aliases() {
  local file="$1" name
  [[ -s "$file" ]] || return 0
  while IFS= read -r -d '' name; do
    if [[ "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
      unset "$name" || true
    fi
  done <"$file"
}

args=("$CLI_BIN" "$MODE" --adapter "$ADAPTER")

case "$MODE" in
  build)
    # Credential-free: clear the wrapper-named provider aliases defensively.
    clear_named_aliases "${DEPLOY_PROVIDER_ENV_CLEAR_FILE:-/dev/null}"
    passthrough=()
    read_nul_into passthrough "${DEPLOY_BUILD_ARGS_FILE:-/dev/null}"
    if ((${#passthrough[@]})); then
      args+=(--)
      args+=("${passthrough[@]}")
    fi
    ;;
  deploy)
    # Typed adapter flags (before `--`): --service-id <id>, optional --stage.
    flags=()
    read_nul_into flags "${DEPLOY_FLAGS_FILE:-/dev/null}"
    if ((${#flags[@]})); then
      args+=("${flags[@]}")
    fi
    # Caller passthrough (after `--`): allowlisted deploy-args (e.g. --comment).
    passthrough=()
    read_nul_into passthrough "${DEPLOY_ARGS_FILE:-/dev/null}"
    if ((${#passthrough[@]})); then
      args+=(--)
      args+=("${passthrough[@]}")
    fi
    ;;
esac

if [[ -n "$MANIFEST" ]]; then
  export EDGEZERO_MANIFEST="$MANIFEST"
else
  unset EDGEZERO_MANIFEST || true
fi

cd "$WORKING_DIRECTORY"
echo "[edgezero-action] running $CLI_BIN $MODE for adapter $ADAPTER"
"${args[@]}"
