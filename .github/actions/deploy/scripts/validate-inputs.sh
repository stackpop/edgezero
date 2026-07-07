#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

ADAPTER=${INPUT_ADAPTER:-}
BUILD_MODE=${INPUT_BUILD_MODE:-auto}
CACHE=${INPUT_CACHE:-false}
BUILD_ARGS=${INPUT_BUILD_ARGS:-[]}
DEPLOY_ARGS=${INPUT_DEPLOY_ARGS:-[]}
FASTLY_API_TOKEN_PRESENT=${INPUT_FASTLY_API_TOKEN_PRESENT:-false}
FASTLY_SERVICE_ID=${INPUT_FASTLY_SERVICE_ID:-}
RUNNER_OS_VALUE=${EDGEZERO_RUNNER_OS:-}
RUNNER_ARCH_VALUE=${EDGEZERO_RUNNER_ARCH:-}

if [[ -n "$RUNNER_OS_VALUE" || -n "$RUNNER_ARCH_VALUE" ]]; then
  [[ "$RUNNER_OS_VALUE" == "Linux" && "$RUNNER_ARCH_VALUE" == "X64" ]] || \
    fail "EdgeZero deploy v0 supports only Linux x86-64 runners; received ${RUNNER_OS_VALUE:-unknown}/${RUNNER_ARCH_VALUE:-unknown}"
fi

[[ -n "$ADAPTER" ]] || fail "input 'adapter' is required; v0 supports only 'fastly'"
case "$ADAPTER" in
  fastly) ;;
  axum) fail "adapter 'axum' has no EdgeZero remote deployment contract" ;;
  cloudflare|spin) fail "adapter '$ADAPTER' is planned for future work but is not implemented in v0; v0 supports only 'fastly'" ;;
  *) fail "unsupported adapter '$ADAPTER'; v0 supports only 'fastly'" ;;
esac

case "$BUILD_MODE" in
  auto|always|never) ;;
  *) fail "input 'build-mode' must be one of: auto, always, never" ;;
esac

case "$CACHE" in
  true|false) ;;
  *) fail "input 'cache' must be exactly 'true' or 'false'" ;;
esac

case "$FASTLY_API_TOKEN_PRESENT" in
  true|false) ;;
  *) fail "internal credential-presence value for fastly-api-token must be exactly 'true' or 'false'" ;;
esac
[[ "$FASTLY_API_TOKEN_PRESENT" == "true" ]] || fail "missing required input 'fastly-api-token'"
[[ -n "$FASTLY_SERVICE_ID" ]] || fail "missing required input 'fastly-service-id'"
if [[ "$FASTLY_SERVICE_ID" == *[[:cntrl:]]* ]]; then
  fail "input 'fastly-service-id' must not contain control characters"
fi

require_cmd jq

parse_args() {
  local name="$1"
  local value="$2"
  local out="$3"
  if ! printf '%s' "$value" | jq -e 'type == "array"' >/dev/null 2>&1; then
    fail "input '$name' must be a JSON array of strings"
  fi
  if ! printf '%s' "$value" | jq -e 'all(.[]; type == "string")' >/dev/null; then
    fail "every element of input '$name' must be a string"
  fi
  if printf '%s' "$value" | jq -e 'any(.[]; contains("\u0000"))' >/dev/null; then
    fail "input '$name' contains a NUL byte, which cannot be passed as an OS argument"
  fi
  printf '%s' "$value" | jq -jr '.[] | ., "\u0000"' >"$out"
}

STATE_DIR=${EDGEZERO_ACTION_STATE_DIR:-${RUNNER_TEMP:-/tmp}/edgezero-action-state}
mkdir -p "$STATE_DIR"
BUILD_ARGS_FILE="$STATE_DIR/build-args.nul"
DEPLOY_ARGS_FILE="$STATE_DIR/deploy-args.nul"
parse_args "build-args" "$BUILD_ARGS" "$BUILD_ARGS_FILE"
parse_args "deploy-args" "$DEPLOY_ARGS" "$DEPLOY_ARGS_FILE"

validate_deploy_args_allowlist() {
  local file="$1"
  local item
  local expect_comment_value=false
  local position=0
  while IFS= read -r -d '' item; do
    position=$((position + 1))
    if [[ "$expect_comment_value" == "true" ]]; then
      expect_comment_value=false
      continue
    fi
    case "$item" in
      --comment=*) ;;
      --comment) expect_comment_value=true ;;
      *) fail "deploy-args allows only Fastly comment flags ('--comment VALUE' or '--comment=VALUE'); rejected argument $position" ;;
    esac
  done <"$file"
  [[ "$expect_comment_value" == "false" ]] || fail "deploy-args '--comment' must be followed by a value"
}
validate_deploy_args_allowlist "$DEPLOY_ARGS_FILE"

append_output adapter fastly
append_output build-args-file "$BUILD_ARGS_FILE"
append_output deploy-args-file "$DEPLOY_ARGS_FILE"
append_output requested-build-mode "$BUILD_MODE"
append_output cache "$CACHE"
