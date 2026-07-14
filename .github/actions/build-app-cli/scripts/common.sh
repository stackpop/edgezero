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
