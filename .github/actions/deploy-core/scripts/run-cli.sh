#!/usr/bin/env bash
set -euo pipefail

# Runs the application CLI (build or deploy) through Bash arrays — never eval.
#
# Provider-neutral: it invokes `<cli-bin> <mode> --adapter <adapter>` with the
# wrapper's typed deploy-flags (before `--`) and the caller's passthrough
# deploy-args (after `--`). Credential scoping is the wrapper's job, done with
# step-level `env:`; this script only clears the wrapper-named aliases during a
# credential-free build.
#
# Inputs (environment):
#   EDGEZERO_CLI_BIN            required  binary name to invoke (on PATH)
#   EDGEZERO_ADAPTER           required  adapter passed as --adapter
#   EDGEZERO_WORKING_DIRECTORY required  directory to run the CLI from
#   EDGEZERO_MANIFEST_PATH     optional  exported as EDGEZERO_MANIFEST when set
#   DEPLOY_BUILD_ARGS_FILE     optional  NUL-delimited build passthrough (build)
#   DEPLOY_FLAGS_FILE          optional  NUL-delimited typed flags     (deploy)
#   DEPLOY_ARGS_FILE           optional  NUL-delimited passthrough     (deploy)
#   DEPLOY_PROVIDER_ENV_CLEAR_FILE optional NUL-delimited env names to clear (build)

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
    deploy) build_deploy_argv "$cli_bin" "$adapter" ;;
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
