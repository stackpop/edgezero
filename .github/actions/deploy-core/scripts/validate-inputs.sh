#!/usr/bin/env bash
set -euo pipefail

# Provider-neutral input validation for the deploy engine. Parses the JSON-array
# parameters into NUL-delimited files, applies the wrapper-supplied deploy-arg
# allowlist, and validates booleans. It never learns provider credential names
# or provider CLI flags — those arrive from the wrapper as opaque data.
#
# Inputs (environment): EDGEZERO__INPUT__ADAPTER, EDGEZERO__INPUT__BUILD_MODE, EDGEZERO__INPUT__CACHE,
# EDGEZERO__INPUT__BUILD_ARGS, EDGEZERO__INPUT__DEPLOY_ARGS, EDGEZERO__INPUT__DEPLOY_ARGS_PREPEND,
# EDGEZERO__INPUT__DEPLOY_FLAGS, EDGEZERO__INPUT__PROVIDER_ENV_CLEAR, EDGEZERO__INPUT__DEPLOY_ARG_ALLOW,
# EDGEZERO__RUNNER__OS/ARCH.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

# Fail unless the runner is the tested Linux x86-64 environment.
require_supported_runner() {
  local os="$1" arch="$2"
  [[ -z "$os" && -z "$arch" ]] && return 0
  [[ "$os" == "Linux" && "$arch" == "X64" ]] ||
    fail "the EdgeZero deploy engine supports only Linux x86-64 runners; received ${os:-unknown}/${arch:-unknown}"
}

# Parse a JSON string array into a NUL-delimited file, rejecting non-arrays,
# non-string entries, and embedded NUL bytes.
parse_json_string_array() {
  local name="$1" value="$2" out_file="$3"
  printf '%s' "$value" | jq -e 'type == "array"' >/dev/null 2>&1 ||
    fail "parameter '$name' must be a JSON array of strings"
  printf '%s' "$value" | jq -e 'all(.[]; type == "string")' >/dev/null ||
    fail "every element of parameter '$name' must be a string"
  if printf '%s' "$value" | jq -e 'any(.[]; contains("\u0000"))' >/dev/null; then
    fail "parameter '$name' contains a NUL byte, which cannot be passed as an OS argument"
  fi
  printf '%s' "$value" | jq -jr '.[] | ., "\u0000"' >"$out_file"
}

# Enforce the wrapper's deploy-arg allowlist. Each permitted flag accepts either
# `--flag=value` (one token) or `--flag value` (two tokens).
enforce_deploy_arg_allowlist() {
  local args_file="$1" allow_list="$2"
  local -a permitted=()
  read -r -a permitted <<<"$allow_list"

  local arg position=0 expecting_value=false
  while IFS= read -r -d '' arg; do
    position=$((position + 1))
    if [[ "$expecting_value" == "true" ]]; then
      expecting_value=false
      continue
    fi
    local flag="${arg%%=*}" matched=false candidate
    for candidate in "${permitted[@]}"; do
      if [[ "$flag" == "$candidate" ]]; then
        matched=true
        [[ "$arg" == *=* ]] || expecting_value=true
        break
      fi
    done
    [[ "$matched" == "true" ]] ||
      fail "deploy-args allows only: ${allow_list:-<none>} (as '--flag value' or '--flag=value'); rejected argument $position"
  done <"$args_file"
  [[ "$expecting_value" == "false" ]] || fail "a value-taking deploy-arg flag is missing its value"
}

main() {
  local adapter="${EDGEZERO__INPUT__ADAPTER:-}"
  local build_mode="${EDGEZERO__INPUT__BUILD_MODE:-auto}"
  local cache="${EDGEZERO__INPUT__CACHE:-false}"
  local stage="${EDGEZERO__INPUT__STAGE:-false}"
  local deploy_arg_allow="${EDGEZERO__INPUT__DEPLOY_ARG_ALLOW:-}"

  require_supported_runner "${EDGEZERO__RUNNER__OS:-}" "${EDGEZERO__RUNNER__ARCH:-}"

  # Well-formedness only: the CLI decides whether the adapter is supported.
  [[ -n "$adapter" ]] || fail "internal parameter 'adapter' is required"
  [[ "$adapter" =~ ^[a-z][a-z0-9-]*$ ]] || fail "adapter '$adapter' is malformed; expected a lowercase token like 'fastly'"

  case "$build_mode" in
    auto | always | never) ;;
    *) fail "input 'build-mode' must be one of: auto, always, never" ;;
  esac
  case "$cache" in
    true | false) ;;
    *) fail "input 'cache' must be exactly 'true' or 'false'" ;;
  esac
  # A typo here must never silently fall back to a production deploy.
  case "$stage" in
    true | false) ;;
    *) fail "input 'stage' must be exactly 'true' or 'false'" ;;
  esac

  require_cmd jq

  local state_dir="${EDGEZERO__ACTION__STATE_DIR:-${RUNNER_TEMP:-/tmp}/edgezero-action-state}"
  mkdir -p "$state_dir"
  local build_args_file="$state_dir/build-args.nul"
  local deploy_args_file="$state_dir/deploy-args.nul"
  local deploy_flags_file="$state_dir/deploy-flags.nul"
  local provider_env_clear_file="$state_dir/provider-env-clear.nul"

  parse_json_string_array "build-args" "${EDGEZERO__INPUT__BUILD_ARGS:-[]}" "$build_args_file"
  parse_json_string_array "deploy-args" "${EDGEZERO__INPUT__DEPLOY_ARGS:-[]}" "$deploy_args_file"
  parse_json_string_array "deploy-flags" "${EDGEZERO__INPUT__DEPLOY_FLAGS:-[]}" "$deploy_flags_file"
  parse_json_string_array "provider-env-clear" "${EDGEZERO__INPUT__PROVIDER_ENV_CLEAR:-[]}" "$provider_env_clear_file"

  # The allowlist governs CALLER deploy-args only.
  enforce_deploy_arg_allowlist "$deploy_args_file" "$deploy_arg_allow"

  # Action-owned passthrough args are prepended AFTER the allowlist check,
  # because they are not caller input — the wrapper supplies them to make the
  # deploy safe in CI (for Fastly: `--non-interactive`, which the built-in
  # deploy path adds for itself but a manifest `[adapters.fastly.commands]
  # deploy = "fastly compute deploy"` override would otherwise never get, so the
  # deploy could block on a TTY prompt). They go first so a caller arg can still
  # override them where the provider CLI takes last-wins.
  local prepend_file="$state_dir/deploy-args-prepend.nul"
  parse_json_string_array "deploy-args-prepend" "${EDGEZERO__INPUT__DEPLOY_ARGS_PREPEND:-[]}" "$prepend_file"
  if [[ -s "$prepend_file" ]]; then
    cat "$prepend_file" "$deploy_args_file" >"$deploy_args_file.merged"
    mv "$deploy_args_file.merged" "$deploy_args_file"
  fi

  append_output adapter "$adapter"
  append_output build-args-file "$build_args_file"
  append_output deploy-args-file "$deploy_args_file"
  append_output deploy-flags-file "$deploy_flags_file"
  append_output provider-env-clear-file "$provider_env_clear_file"
  append_output requested-build-mode "$build_mode"
  append_output cache "$cache"
}

main "$@"
