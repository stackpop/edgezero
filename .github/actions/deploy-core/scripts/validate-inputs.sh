#!/usr/bin/env bash
set -euo pipefail

# Provider-neutral input validation for the deploy engine. Parses the JSON-array
# parameters into NUL-delimited files, applies the wrapper-supplied deploy-arg
# allowlist, and validates booleans. It never learns provider credential names
# or provider CLI flags — those arrive from the wrapper as opaque data.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

ADAPTER=${INPUT_ADAPTER:-}
BUILD_MODE=${INPUT_BUILD_MODE:-auto}
CACHE=${INPUT_CACHE:-false}
BUILD_ARGS=${INPUT_BUILD_ARGS:-[]}
DEPLOY_ARGS=${INPUT_DEPLOY_ARGS:-[]}
DEPLOY_FLAGS=${INPUT_DEPLOY_FLAGS:-[]}
PROVIDER_ENV_CLEAR=${INPUT_PROVIDER_ENV_CLEAR:-[]}
# Space-separated list of value-taking flags the caller's deploy-args may use
# (wrapper-supplied). Empty means no caller deploy-args are permitted.
DEPLOY_ARG_ALLOW=${INPUT_DEPLOY_ARG_ALLOW:-}
RUNNER_OS_VALUE=${EDGEZERO_RUNNER_OS:-}
RUNNER_ARCH_VALUE=${EDGEZERO_RUNNER_ARCH:-}

if [[ -n "$RUNNER_OS_VALUE" || -n "$RUNNER_ARCH_VALUE" ]]; then
  [[ "$RUNNER_OS_VALUE" == "Linux" && "$RUNNER_ARCH_VALUE" == "X64" ]] ||
    fail "the EdgeZero deploy engine supports only Linux x86-64 runners; received ${RUNNER_OS_VALUE:-unknown}/${RUNNER_ARCH_VALUE:-unknown}"
fi

# Well-formedness only: the CLI validates whether the adapter is actually
# supported (there is no engine allowlist).
[[ -n "$ADAPTER" ]] || fail "internal parameter 'adapter' is required"
[[ "$ADAPTER" =~ ^[a-z][a-z0-9-]*$ ]] || fail "adapter '$ADAPTER' is malformed; expected a lowercase token like 'fastly'"

case "$BUILD_MODE" in
  auto | always | never) ;;
  *) fail "input 'build-mode' must be one of: auto, always, never" ;;
esac

case "$CACHE" in
  true | false) ;;
  *) fail "input 'cache' must be exactly 'true' or 'false'" ;;
esac

require_cmd jq

parse_args() {
  local name="$1" value="$2" out="$3"
  if ! printf '%s' "$value" | jq -e 'type == "array"' >/dev/null 2>&1; then
    fail "parameter '$name' must be a JSON array of strings"
  fi
  if ! printf '%s' "$value" | jq -e 'all(.[]; type == "string")' >/dev/null; then
    fail "every element of parameter '$name' must be a string"
  fi
  if printf '%s' "$value" | jq -e 'any(.[]; contains("\u0000"))' >/dev/null; then
    fail "parameter '$name' contains a NUL byte, which cannot be passed as an OS argument"
  fi
  printf '%s' "$value" | jq -jr '.[] | ., "\u0000"' >"$out"
}

STATE_DIR=${EDGEZERO_ACTION_STATE_DIR:-${RUNNER_TEMP:-/tmp}/edgezero-action-state}
mkdir -p "$STATE_DIR"
BUILD_ARGS_FILE="$STATE_DIR/build-args.nul"
DEPLOY_ARGS_FILE="$STATE_DIR/deploy-args.nul"
DEPLOY_FLAGS_FILE="$STATE_DIR/deploy-flags.nul"
PROVIDER_ENV_CLEAR_FILE="$STATE_DIR/provider-env-clear.nul"
parse_args "build-args" "$BUILD_ARGS" "$BUILD_ARGS_FILE"
parse_args "deploy-args" "$DEPLOY_ARGS" "$DEPLOY_ARGS_FILE"
parse_args "deploy-flags" "$DEPLOY_FLAGS" "$DEPLOY_FLAGS_FILE"
parse_args "provider-env-clear" "$PROVIDER_ENV_CLEAR" "$PROVIDER_ENV_CLEAR_FILE"

# Apply the wrapper-supplied deploy-arg allowlist. Each permitted flag accepts
# either `--flag=value` (one token) or `--flag value` (two tokens).
validate_deploy_args_allowlist() {
  local file="$1"
  local -a allowed=()
  read -r -a allowed <<<"$DEPLOY_ARG_ALLOW"
  local item position=0 expect_value=false
  while IFS= read -r -d '' item; do
    position=$((position + 1))
    if [[ "$expect_value" == "true" ]]; then
      expect_value=false
      continue
    fi
    local flag="${item%%=*}"
    local matched=false permitted
    for permitted in "${allowed[@]}"; do
      if [[ "$flag" == "$permitted" ]]; then
        matched=true
        [[ "$item" == *=* ]] || expect_value=true
        break
      fi
    done
    [[ "$matched" == "true" ]] ||
      fail "deploy-args allows only: ${DEPLOY_ARG_ALLOW:-<none>} (as '--flag value' or '--flag=value'); rejected argument $position"
  done <"$file"
  [[ "$expect_value" == "false" ]] || fail "a value-taking deploy-arg flag is missing its value"
}
validate_deploy_args_allowlist "$DEPLOY_ARGS_FILE"

append_output adapter "$ADAPTER"
append_output build-args-file "$BUILD_ARGS_FILE"
append_output deploy-args-file "$DEPLOY_ARGS_FILE"
append_output deploy-flags-file "$DEPLOY_FLAGS_FILE"
append_output provider-env-clear-file "$PROVIDER_ENV_CLEAR_FILE"
append_output requested-build-mode "$BUILD_MODE"
append_output cache "$CACHE"
