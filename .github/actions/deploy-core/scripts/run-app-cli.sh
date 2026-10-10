#!/usr/bin/env bash
set -euo pipefail

# Invokes an already-verified application CLI from an exact NUL-delimited argv.
# Provider wrappers own command construction, credential aliases, and any extra
# public runtime names; this core owns confinement, environment scrubbing,
# mutation signalling, and exact exit-status propagation.
#
# Reads (env):
#   EDGEZERO__ACTION__WORKSPACE             required  trusted root containing the CLI and control files
#   EDGEZERO__APP__CLI__PATH                required  executable regular file beneath the action workspace
#   EDGEZERO__APP__CLI__ARGS_FILE           required  NUL-delimited argv beneath the action workspace
#   EDGEZERO__APP__CLI__MUTATES             required  true | false
#   EDGEZERO__PROJECT__WORKING_DIRECTORY    optional  invocation directory (default: current directory)
#   EDGEZERO__PROJECT__MANIFEST_PATH        optional  exported to the CLI as EDGEZERO_MANIFEST
#   EDGEZERO__PROVIDER__ENV_CLEAR_FILE      optional  NUL-delimited provider aliases allowed for import
#   EDGEZERO__PROVIDER__ENV                 optional  JSON object containing typed provider values
#   EDGEZERO__PUBLIC_RUNTIME_ENV_ALLOW_FILE optional  NUL-delimited provider-specific public EDGEZERO__ names
# Writes (outputs):
#   mutation-attempted                      true immediately before a mutating CLI invocation
# Otherwise preserves the application CLI's stdout, stderr, and exit status.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

COLLECTED=()
validate_nul_file() {
  local file="$1" label="$2"
  [[ -f "$file" && ! -L "$file" ]] || fail "$label must be a regular file"
  [[ ! -s "$file" ]] ||
    [[ "$(tail -c 1 "$file" | od -An -tu1 | tr -d ' ')" == 0 ]] ||
    fail "$label must end in NUL"
}

collect_nul() {
  local file="$1" entry
  COLLECTED=()
  validate_nul_file "$file" "application CLI argument file"
  [[ -s "$file" ]] || return 0
  while IFS= read -r -d '' entry; do COLLECTED+=("$entry"); done <"$file"
}

clear_named_aliases() {
  local file="$1" name
  [[ -s "$file" ]] || return 0
  while IFS= read -r -d '' name; do
    [[ "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || fail "provider clear-list contains an invalid name"
    unset "$name" || true
  done <"$file"
}

name_in_nul_file() {
  local wanted="$1" file="$2" name
  [[ -s "$file" ]] || return 1
  while IFS= read -r -d '' name; do [[ "$name" == "$wanted" ]] && return 0; done <"$file"
  return 1
}

import_provider_env() {
  local clear_file="$1" json="${EDGEZERO__PROVIDER__ENV:-}" name b64 value
  [[ -n "$json" ]] || json='{}'
  clear_named_aliases "$clear_file"
  require_cmd jq
  require_cmd base64
  printf '%s' "$json" | jq -e 'type == "object" and all(.[]; type == "string" and ((contains("\u0000") or contains("\n") or contains("\r")) | not))' >/dev/null 2>&1 ||
    fail "EDGEZERO__PROVIDER__ENV must be a JSON object of string values without NUL, CR, or LF"
  while read -r name b64; do
    [[ "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || fail "provider environment name is invalid"
    name_in_nul_file "$name" "$clear_file" || fail "provider environment name must appear in the clear-list"
    value=$(printf '%s' "$b64" | base64 --decode)
    export "$name=$value"
  done < <(printf '%s' "$json" | jq -r 'to_entries[] | "\(.key) \(.value | @base64)"')
}

PUBLIC_NAMES=()
PUBLIC_VALUES=()
capture_public_env() {
  local allow_file="$1" name allowed upper_name
  while IFS= read -r name; do
    allowed=false
    upper_name=""
    if [[ "$name" == "EDGEZERO__ADAPTER__HOST" ]] ||
      [[ "$name" == "EDGEZERO__ADAPTER__PORT" ]] ||
      [[ "$name" == "EDGEZERO__LOGGING__ENDPOINT" ]] ||
      [[ "$name" == "EDGEZERO__LOGGING__LEVEL" ]] ||
      [[ "$name" =~ ^EDGEZERO__STORES__CONFIG__[A-Z0-9_]+__(NAME|KEY)$ ]] ||
      [[ "$name" =~ ^EDGEZERO__STORES__(KV|SECRETS)__[A-Z0-9_]+__NAME$ ]] ||
      name_in_nul_file "$name" "$allow_file"; then
      allowed=true
    fi
    if [[ "$allowed" == true ]]; then
      PUBLIC_NAMES+=("$name")
      PUBLIC_VALUES+=("${!name}")
    else
      upper_name=$(printf '%s' "$name" | tr '[:lower:]' '[:upper:]')
    fi
    if [[ "$allowed" != true && "$upper_name" == EDGEZERO__STORES__* ]]; then
      fail "store selector $name must use the canonical upper-case form"
    fi
  done < <(compgen -e)
}

scrub_private_env() {
  local name index
  while IFS= read -r name; do
    if [[ "$name" == EDGEZERO__* ]]; then unset "$name" || true; fi
  done < <(compgen -e)
  for ((index = 0; index < ${#PUBLIC_NAMES[@]}; index++)); do
    name="${PUBLIC_NAMES[$index]}"
    export "$name=${PUBLIC_VALUES[$index]}"
  done
}

main() {
  local cli="${EDGEZERO__APP__CLI__PATH:-}"
  local args_file="${EDGEZERO__APP__CLI__ARGS_FILE:-}"
  local mutates="${EDGEZERO__APP__CLI__MUTATES:-}"
  local workspace="${EDGEZERO__ACTION__WORKSPACE:-}"
  local working_directory="${EDGEZERO__PROJECT__WORKING_DIRECTORY:-$PWD}"
  local manifest="${EDGEZERO__PROJECT__MANIFEST_PATH:-}"
  local clear_file="${EDGEZERO__PROVIDER__ENV_CLEAR_FILE:-/dev/null}"
  local allow_file="${EDGEZERO__PUBLIC_RUNTIME_ENV_ALLOW_FILE:-/dev/null}"
  require_input application-cli "$cli"
  require_input application-cli-args-file "$args_file"
  [[ -f "$cli" && ! -L "$cli" && -x "$cli" ]] || fail "application CLI must be an executable regular file"
  [[ -d "$workspace" ]] || fail "action workspace must be a directory"
  local workspace_real
  workspace_real=$(canonical_path "$workspace")
  local cli_real
  cli_real=$(canonical_path "$cli")
  is_under "$workspace_real" "$cli_real" ||
    fail "application CLI must resolve beneath the action workspace"
  local file real
  for file in "$args_file" "$clear_file" "$allow_file"; do
    [[ "$file" == /dev/null ]] && continue
    [[ -f "$file" && ! -L "$file" ]] || fail "application CLI control file must be a regular file"
    real=$(canonical_path "$file")
    is_under "$workspace_real" "$real" || fail "application CLI control file must resolve beneath the action workspace"
  done
  [[ "$clear_file" == /dev/null ]] || validate_nul_file "$clear_file" "provider clear-list"
  [[ "$allow_file" == /dev/null ]] || validate_nul_file "$allow_file" "public runtime allow-list"
  case "$mutates" in true | false) ;; *) fail "application CLI mutation setting must be true or false" ;; esac
  [[ -d "$working_directory" ]] || fail "application CLI working directory must exist"
  collect_nul "$args_file"
  local -a argv=("$cli" "${COLLECTED[@]}")
  import_provider_env "$clear_file"
  capture_public_env "$allow_file"
  scrub_private_env
  if [[ -n "$manifest" ]]; then export EDGEZERO_MANIFEST="$manifest"; else unset EDGEZERO_MANIFEST || true; fi
  cd "$working_directory"
  [[ "$mutates" == false ]] || append_output mutation-attempted true
  echo "[edgezero-action] running verified application CLI" >&2
  "${argv[@]}"
}

main "$@"
