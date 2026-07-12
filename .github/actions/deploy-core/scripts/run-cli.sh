#!/usr/bin/env bash
set -euo pipefail

# Runs the application CLI (build or deploy) through Bash arrays — never eval.
#
# Provider-neutral: it invokes `<cli-bin> <mode> --adapter <adapter>` with the
# wrapper's typed deploy-flags (before `--`) and the caller's passthrough
# deploy-args (after `--`).
#
# Credential boundary (deploy mode): the wrapper never exports provider tokens
# onto the step directly. It passes DEPLOY_PROVIDER_ENV (a JSON object of typed
# credential name -> value) plus a provider-env-clear name list. This script
# first UNSETS every clear-listed alias (removing any inherited FASTLY_* value),
# then exports only the typed values from DEPLOY_PROVIDER_ENV — and only names
# that are declared in the clear list. So inherited endpoint/token aliases can
# never survive into the deploy. Build mode is credential-free and only clears.
#
# Inputs (environment):
#   EDGEZERO_CLI_BIN            required  binary name to invoke (on PATH)
#   EDGEZERO_ADAPTER           required  adapter passed as --adapter
#   EDGEZERO_WORKING_DIRECTORY required  directory to run the CLI from
#   EDGEZERO_MANIFEST_PATH     optional  exported as EDGEZERO_MANIFEST when set
#   DEPLOY_BUILD_ARGS_FILE     optional  NUL-delimited build passthrough (build)
#   DEPLOY_FLAGS_FILE          optional  NUL-delimited typed flags     (deploy)
#   DEPLOY_ARGS_FILE           optional  NUL-delimited passthrough     (deploy)
#   DEPLOY_PROVIDER_ENV_CLEAR_FILE optional NUL-delimited env names to clear
#   DEPLOY_PROVIDER_ENV        optional  JSON object of typed creds     (deploy)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

# Collect a NUL-delimited file into the global COLLECTED array (portable; avoids
# Bash 4.3 namerefs, which some runners/macOS Bash 3.2 lack).
COLLECTED=()
collect_nul() {
  local file="$1"
  COLLECTED=()
  [[ -s "$file" ]] || return 0
  local entry
  while IFS= read -r -d '' entry; do
    COLLECTED+=("$entry")
  done <"$file"
}

# Unset each wrapper-named provider alias listed (NUL-delimited) in a file.
clear_named_aliases() {
  local file="$1"
  [[ -s "$file" ]] || return 0
  local name
  while IFS= read -r -d '' name; do
    if [[ "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
      unset "$name" || true
    fi
  done <"$file"
}

# Return 0 if <name> appears in the NUL-delimited clear-list file.
name_in_clear_list() {
  local wanted="$1" file="$2" name
  [[ -s "$file" ]] || return 1
  while IFS= read -r -d '' name; do
    [[ "$name" == "$wanted" ]] && return 0
  done <"$file"
  return 1
}

# Clear the provider aliases, then export ONLY the typed values from
# DEPLOY_PROVIDER_ENV whose names are declared in the clear list. jq parses the
# JSON, so values are opaque data (never interpreted by the shell).
import_provider_env() {
  local clear_file="$1"
  local json="${DEPLOY_PROVIDER_ENV:-}"
  [[ -n "$json" ]] || json='{}'
  clear_named_aliases "$clear_file"

  require_cmd jq
  require_cmd base64
  printf '%s' "$json" | jq -e 'type == "object"' >/dev/null 2>&1 ||
    fail "DEPLOY_PROVIDER_ENV must be a JSON object of string values"
  printf '%s' "$json" | jq -e 'all(.[]; type == "string")' >/dev/null 2>&1 ||
    fail "every DEPLOY_PROVIDER_ENV value must be a string"

  # One "NAME BASE64VALUE" line per entry. Base64 keeps values line-safe
  # (newlines, spaces, quotes cannot break the read loop) and opaque.
  local name b64 value
  while read -r name b64; do
    [[ "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] ||
      fail "DEPLOY_PROVIDER_ENV name '$name' is not a valid environment variable name"
    name_in_clear_list "$name" "$clear_file" ||
      fail "DEPLOY_PROVIDER_ENV name '$name' must be declared in provider-env-clear"
    value=$(printf '%s' "$b64" | base64 --decode)
    export "$name=$value"
  done < <(printf '%s' "$json" | jq -r 'to_entries[] | "\(.key) \(.value | @base64)"')
}

# Build the CLI argv for `build` mode into the global ARGV array.
build_build_argv() {
  local cli_bin="$1"
  local adapter="$2"
  ARGV=("$cli_bin" build --adapter "$adapter")
  # Credential-free build: defensively drop the wrapper-named aliases.
  clear_named_aliases "${DEPLOY_PROVIDER_ENV_CLEAR_FILE:-/dev/null}"
  collect_nul "${DEPLOY_BUILD_ARGS_FILE:-/dev/null}"
  if ((${#COLLECTED[@]})); then
    ARGV+=(-- "${COLLECTED[@]}")
  fi
}

# Build the CLI argv for `deploy` mode into the global ARGV array.
build_deploy_argv() {
  local cli_bin="$1"
  local adapter="$2"
  ARGV=("$cli_bin" deploy --adapter "$adapter")
  # Typed adapter flags (before `--`): e.g. --service-id <id>, --stage.
  collect_nul "${DEPLOY_FLAGS_FILE:-/dev/null}"
  ((${#COLLECTED[@]})) && ARGV+=("${COLLECTED[@]}")
  # Caller passthrough (after `--`): allowlisted deploy-args, e.g. --comment.
  collect_nul "${DEPLOY_ARGS_FILE:-/dev/null}"
  if ((${#COLLECTED[@]})); then
    ARGV+=(-- "${COLLECTED[@]}")
  fi
}

ARGV=()
main() {
  local mode="${1:-}"
  case "$mode" in
    build | deploy) ;;
    *) fail "usage: run-cli.sh build|deploy" ;;
  esac

  local cli_bin="${EDGEZERO_CLI_BIN:?EDGEZERO_CLI_BIN is required}"
  local adapter="${EDGEZERO_ADAPTER:?EDGEZERO_ADAPTER is required}"
  local working_directory="${EDGEZERO_WORKING_DIRECTORY:?EDGEZERO_WORKING_DIRECTORY is required}"
  local manifest="${EDGEZERO_MANIFEST_PATH:-}"
  require_cmd "$cli_bin"

  case "$mode" in
    build) build_build_argv "$cli_bin" "$adapter" ;;
    deploy)
      # Clear inherited provider aliases and export only the typed credentials.
      import_provider_env "${DEPLOY_PROVIDER_ENV_CLEAR_FILE:-/dev/null}"
      build_deploy_argv "$cli_bin" "$adapter"
      ;;
  esac

  if [[ -n "$manifest" ]]; then
    export EDGEZERO_MANIFEST="$manifest"
  else
    unset EDGEZERO_MANIFEST || true
  fi

  cd "$working_directory"
  echo "[edgezero-action] running $cli_bin $mode for adapter $adapter" >&2
  "${ARGV[@]}"
}

main "$@"
