#!/usr/bin/env bash
set -euo pipefail

# Sourced helper library for build-app-cli. Defines the shared shell helpers
# (annotations, output writers, artifact-name and owned-dir guards). It is never
# executed directly and reads no environment of its own; callers source it right
# after their own strict-mode preamble.

escape_annotation() {
  local value="$*"
  value=${value//%/%25}
  value=${value//$'\r'/%0D}
  value=${value//$'\n'/%0A}
  printf '%s' "$value"
}

fail() {
  local message
  message=$(escape_annotation "$*")
  echo "::error::$message" >&2
  exit 1
}

notice() {
  local message
  message=$(escape_annotation "$*")
  echo "::notice::$message" >&2
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "required command '$1' was not found"
}

append_output() {
  local name="$1"
  local value="$2"
  [[ "$name" =~ ^[A-Za-z_][A-Za-z0-9_-]*$ ]] || fail "invalid output name '$name'"
  if [[ "$value" == *$'\n'* || "$value" == *$'\r'* ]]; then
    fail "output '$name' contains a newline or carriage return"
  fi
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    printf '%s=%s\n' "$name" "$value" >>"$GITHUB_OUTPUT"
  else
    printf '%s=%s\n' "$name" "$value"
  fi
}

append_env() {
  local name="$1"
  local value="$2"
  [[ "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || fail "invalid environment name '$name'"
  if [[ "$value" == *$'\n'* || "$value" == *$'\r'* ]]; then
    fail "environment value '$name' contains a newline or carriage return"
  fi
  if [[ -n "${GITHUB_ENV:-}" ]]; then
    printf '%s=%s\n' "$name" "$value" >>"$GITHUB_ENV"
  else
    export "$name=$value"
  fi
}

canonical_path() {
  require_cmd realpath
  local path
  path=$(realpath "$1" 2>/dev/null) || fail "could not resolve path '$1'"
  printf '%s\n' "$path"
}

relative_to() {
  local root="${1%/}"
  local path="${2%/}"
  if [[ "$path" == "$root" ]]; then
    printf '.\n'
  elif [[ "$path" == "$root"/* ]]; then
    printf '%s\n' "${path#"$root"/}"
  else
    printf '%s\n' "$path"
  fi
}

is_under() {
  local root="${1%/}"
  local path="${2%/}"
  [[ "$path" == "$root" || "$path" == "$root"/* ]]
}

json_get() {
  require_cmd jq
  jq -er ".$2" "$1"
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{ print $1 }'
  else
    fail "required command 'sha256sum' or 'shasum' was not found"
  fi
}

read_tool_version() {
  local file="$1"
  local tool="$2"
  awk -v tool="$tool" '$1 == tool { print $2; found=1; exit } END { if (!found) exit 1 }' "$file"
}

sanitize_ref() {
  printf '%s' "$1" | tr -c 'A-Za-z0-9_.=-' '-'
}

# Validate the `provider-env-clear` input and print the credential alias names it
# contains, one per line.
#
# Fails CLOSED on anything that is not a JSON array of non-empty shell-identifier
# strings. This matters because a permissive parse silently clears NOTHING: with
# `jq '.[]?'`, the values `"FASTLY_API_TOKEN"`, `{}`, `null` and `123` all exit 0
# and yield no names, so a typo in the input would leave every inherited
# credential in scope for app-controlled code while appearing to succeed.
provider_env_clear_names() {
  local json="${1-}"
  require_cmd jq
  local kind
  kind=$(printf '%s' "$json" | jq -r 'type' 2>/dev/null) ||
    fail "input 'provider-env-clear' is not valid JSON: '$json'"
  [[ "$kind" == "array" ]] ||
    fail "input 'provider-env-clear' must be a JSON ARRAY of environment variable names, got $kind: '$json'"
  local malformed
  malformed=$(printf '%s' "$json" | jq -r '[.[] | select((type != "string") or (. == ""))] | length')
  [[ "$malformed" == "0" ]] ||
    fail "input 'provider-env-clear' must contain only non-empty strings: '$json'"

  local name
  while IFS= read -r name; do
    [[ -n "$name" ]] || continue
    [[ "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] ||
      fail "input 'provider-env-clear' contains an invalid environment variable name: '$name'"
    printf '%s\n' "$name"
  done < <(printf '%s' "$json" | jq -r '.[]')
}

# Re-exec this script with the named provider credentials REMOVED from the
# process environment, then continue.
#
# `unset` alone is not sufficient. On Linux, `/proc/<pid>/environ` exposes the
# environment block a process was `execve`d with, and later `unset`/`setenv` do
# not rewrite it — so an app-controlled Cargo build script (or the built CLI's
# `--help`) could read the token straight out of this shell's `/proc` entry even
# after we unset it. Replacing the process image via `env -u ... exec` gives THIS
# process, its `/proc` entry, and every descendant a genuinely clean environment.
#
# Provider-NEUTRAL: the names come from the caller's `provider-env-clear` input.
exec_with_cleared_provider_env() {
  local json="$1"
  shift
  # COMMAND substitution, not process substitution: a validation failure inside
  # `provider_env_clear_names` must abort the build. Under `< <(...)` its `fail`
  # would only kill the subshell, leaving an EMPTY name list here — and we would
  # then exec with nothing stripped, i.e. fail OPEN with the credentials intact.
  local names_raw
  names_raw=$(provider_env_clear_names "$json") || exit 1

  local -a cmd=(env)
  local name
  while IFS= read -r name; do
    [[ -n "$name" ]] || continue
    cmd+=(-u "$name")
  done <<<"$names_raw"
  # Sentinel so the re-exec'd invocation does not loop.
  cmd+=("EDGEZERO__PROVIDER__ENV_CLEARED=1" "$@")
  exec "${cmd[@]}"
}

# Reject an artifact name that could escape the action-owned staging directory
# when used as a path component: no separators, no traversal, no leading dot,
# only a conservative character set.
validate_artifact_name() {
  local name="$1"
  [[ -n "$name" ]] || fail "input 'artifact-name' must not be empty"
  case "$name" in
    */* | *\\* | *..*) fail "input 'artifact-name' must not contain path separators or '..': '$name'" ;;
    .*) fail "input 'artifact-name' must not start with '.': '$name'" ;;
  esac
  [[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] ||
    fail "input 'artifact-name' may contain only letters, digits, '.', '_', '-': '$name'"
}

# Recreate an action-owned scratch directory. Refuses to remove anything not
# beneath the temp root, so a stray/inherited value can never delete the checkout.
reset_owned_dir() {
  local dir="$1" temp_root="$2"
  [[ -n "$dir" && -n "$temp_root" ]] || fail "internal: reset_owned_dir needs a dir and a temp root"
  case "$dir" in
    *..*) fail "refusing to remove '$dir': path traversal" ;;
  esac
  [[ "$dir" == "$temp_root"/* ]] ||
    fail "refusing to remove '$dir': not beneath the action-owned temp root '$temp_root'"
  rm -rf "$dir"
  mkdir -p "$dir"
}
