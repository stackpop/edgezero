#!/usr/bin/env bash
set -euo pipefail

# Runs the application CLI (build or deploy) through Bash arrays — never eval.
#
# Provider-neutral: it invokes `<cli-bin> <mode> --adapter <adapter>` with the
# wrapper's typed deploy-flags (before `--`) and the caller's passthrough
# deploy-args (after `--`).
#
# Credential boundary (deploy mode): the wrapper never exports provider tokens
# onto the step directly. It passes EDGEZERO__PROVIDER__ENV (a JSON object of typed
# credential name -> value) plus a provider-env-clear name list. This script
# first UNSETS every clear-listed alias (removing any inherited FASTLY_* value),
# then exports only the typed values from EDGEZERO__PROVIDER__ENV — and only names
# that are declared in the clear list. So inherited endpoint/token aliases can
# never survive into the deploy. Build mode is credential-free and only clears.
#
# Inputs (environment):
#   EDGEZERO__APP__CLI__BIN                   required binary name to invoke (on PATH)
#   EDGEZERO__ADAPTER                    required adapter passed as --adapter
#   EDGEZERO__PROJECT__WORKING_DIRECTORY required directory to run the CLI from
#   EDGEZERO__PROJECT__MANIFEST_PATH     optional exported as EDGEZERO_MANIFEST when set
#   EDGEZERO__BUILD__ARGS_FILE           optional NUL-delimited build passthrough (build)
#   EDGEZERO__DEPLOY__FLAGS_FILE         optional NUL-delimited typed flags     (deploy)
#   EDGEZERO__DEPLOY__ARGS_FILE          optional NUL-delimited passthrough     (deploy)
#   EDGEZERO__PROVIDER__ENV_CLEAR_FILE   optional NUL-delimited env names to clear
#   EDGEZERO__PROVIDER__ENV              optional JSON object of typed creds     (deploy)

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
# EDGEZERO__PROVIDER__ENV whose names are declared in the clear list. jq parses the
# JSON, so values are opaque data (never interpreted by the shell).
import_provider_env() {
  local clear_file="$1"
  local json="${EDGEZERO__PROVIDER__ENV:-}"
  [[ -n "$json" ]] || json='{}'
  clear_named_aliases "$clear_file"

  require_cmd jq
  require_cmd base64
  printf '%s' "$json" | jq -e 'type == "object"' >/dev/null 2>&1 ||
    fail "EDGEZERO__PROVIDER__ENV must be a JSON object of string values"
  printf '%s' "$json" | jq -e 'all(.[]; type == "string")' >/dev/null 2>&1 ||
    fail "every EDGEZERO__PROVIDER__ENV value must be a string"

  # One "NAME BASE64VALUE" line per entry. Base64 keeps values line-safe
  # (newlines, spaces, quotes cannot break the read loop) and opaque.
  local name b64 value
  while read -r name b64; do
    [[ "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] ||
      fail "EDGEZERO__PROVIDER__ENV name '$name' is not a valid environment variable name"
    name_in_clear_list "$name" "$clear_file" ||
      fail "EDGEZERO__PROVIDER__ENV name '$name' must be declared in provider-env-clear"
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
  clear_named_aliases "${EDGEZERO__PROVIDER__ENV_CLEAR_FILE:-/dev/null}"
  collect_nul "${EDGEZERO__BUILD__ARGS_FILE:-/dev/null}"
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
  collect_nul "${EDGEZERO__DEPLOY__FLAGS_FILE:-/dev/null}"
  ((${#COLLECTED[@]})) && ARGV+=("${COLLECTED[@]}")
  # Caller passthrough (after `--`): allowlisted deploy-args, e.g. --comment.
  collect_nul "${EDGEZERO__DEPLOY__ARGS_FILE:-/dev/null}"
  if ((${#COLLECTED[@]})); then
    ARGV+=(-- "${COLLECTED[@]}")
  fi
}

# Unset the action's PRIVATE environment namespace before handing control to the
# application CLI.
#
# This is a credential boundary, not tidiness. The wrapper carries the typed
# token into this script twice: once as `EDGEZERO__<PROVIDER>_API_TOKEN` (so the
# step's YAML can build the JSON without interpolating a secret into a `run:`
# block), and once inside `EDGEZERO__PROVIDER__ENV` itself. Both are
# secret-bearing. Without this scrub they stay exported, so the app CLI — and
# every subprocess it spawns, including a manifest `[adapters.*.commands]` shell
# command — inherits the raw token under names we never promised, and any
# `env`-dumping build script would print it.
#
# This is why every action-owned variable lives under the double-underscore
# `EDGEZERO__` prefix: the boundary is then one rule with no list to keep in sync.
# `EDGEZERO_MANIFEST` (SINGLE underscore) is deliberately outside it — that is the
# CLI's own public contract, not ours, and it is the one variable we do pass on.
scrub_action_private_env() {
  local name
  while IFS= read -r name; do
    case "$name" in
      EDGEZERO__*) unset "$name" || true ;;
      *) ;;
    esac
  done < <(compgen -e)
}

ARGV=()
main() {
  local mode="${1:-}"
  case "$mode" in
    build | deploy) ;;
    *) fail "usage: run-app-cli.sh build|deploy" ;;
  esac

  local cli_bin="${EDGEZERO__APP__CLI__BIN:?EDGEZERO__APP__CLI__BIN is required}"
  local adapter="${EDGEZERO__ADAPTER:?EDGEZERO__ADAPTER is required}"
  local working_directory="${EDGEZERO__PROJECT__WORKING_DIRECTORY:?EDGEZERO__PROJECT__WORKING_DIRECTORY is required}"
  local manifest="${EDGEZERO__PROJECT__MANIFEST_PATH:-}"
  require_cmd "$cli_bin"

  case "$mode" in
    build) build_build_argv "$cli_bin" "$adapter" ;;
    deploy)
      # Clear inherited provider aliases and export only the typed credentials.
      import_provider_env "${EDGEZERO__PROVIDER__ENV_CLEAR_FILE:-/dev/null}"
      build_deploy_argv "$cli_bin" "$adapter"
      ;;
  esac

  # Everything the action needed from its own env is now in locals or in ARGV.
  scrub_action_private_env

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
