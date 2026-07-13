#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

MODE=${1:?usage: run-edgezero.sh build|deploy}
case "$MODE" in build|deploy) ;; *) fail "mode must be build or deploy" ;; esac

WORKING_DIRECTORY=${WORKING_DIRECTORY:?WORKING_DIRECTORY is required}
MANIFEST=${MANIFEST:-}
require_cmd edgezero

clear_fastly_aliases() {
  unset FASTLY_API_TOKEN FASTLY_SERVICE_ID FASTLY_TOKEN FASTLY_PROFILE \
    FASTLY_API_ENDPOINT FASTLY_ENDPOINT FASTLY_API_URL FASTLY_DEBUG \
    FASTLY_DEBUG_MODE FASTLY_CONFIG_FILE FASTLY_HOME FASTLY_SERVICE_NAME \
    FASTLY_API_KEY FASTLY_AUTH_TOKEN INPUT_FASTLY_API_TOKEN || true
}

case "$MODE" in
  build)
    ARGS_FILE=${ARGS_FILE:?ARGS_FILE is required}
    clear_fastly_aliases
    args=(edgezero build --adapter fastly)
    if [[ -s "$ARGS_FILE" ]]; then
      args+=(--)
      while IFS= read -r -d '' item; do
        args+=("$item")
      done <"$ARGS_FILE"
    fi
    ;;
  deploy)
    ARGS_FILE=${ARGS_FILE:?ARGS_FILE is required}
    FASTLY_API_TOKEN_VALUE=${FASTLY_API_TOKEN:-}
    FASTLY_SERVICE_ID_VALUE=${FASTLY_SERVICE_ID:-}
    [[ -n "$FASTLY_API_TOKEN_VALUE" ]] || fail "missing required deploy environment FASTLY_API_TOKEN"
    [[ -n "$FASTLY_SERVICE_ID_VALUE" ]] || fail "missing required deploy environment FASTLY_SERVICE_ID"
    clear_fastly_aliases
    export FASTLY_API_TOKEN="$FASTLY_API_TOKEN_VALUE"
    export FASTLY_SERVICE_ID="$FASTLY_SERVICE_ID_VALUE"
    args=(edgezero deploy --adapter fastly -- --service-id "$FASTLY_SERVICE_ID_VALUE" --non-interactive)
    if [[ -s "$ARGS_FILE" ]]; then
      while IFS= read -r -d '' item; do
        args+=("$item")
      done <"$ARGS_FILE"
    fi
    ;;
esac

if [[ -n "$MANIFEST" ]]; then
  export EDGEZERO_MANIFEST="$MANIFEST"
else
  unset EDGEZERO_MANIFEST || true
fi

cd "$WORKING_DIRECTORY"
echo "[edgezero-action] running EdgeZero $MODE for adapter fastly"
"${args[@]}"
