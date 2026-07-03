#!/usr/bin/env bash
set -euo pipefail

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
  python3 - "$1" <<'PY'
import os, sys
print(os.path.realpath(sys.argv[1]))
PY
}

relative_to() {
  python3 - "$1" "$2" <<'PY'
import os, sys
try:
    print(os.path.relpath(sys.argv[2], sys.argv[1]))
except ValueError:
    print(sys.argv[2])
PY
}

is_under() {
  python3 - "$1" "$2" <<'PY'
import os, sys
root = os.path.realpath(sys.argv[1])
path = os.path.realpath(sys.argv[2])
try:
    common = os.path.commonpath([root, path])
except ValueError:
    sys.exit(1)
sys.exit(0 if common == root else 1)
PY
}

json_get() {
  python3 - "$1" "$2" <<'PY'
import json, sys
with open(sys.argv[1], encoding='utf-8') as f:
    data = json.load(f)
cur = data
for part in sys.argv[2].split('.'):
    cur = cur[part]
print(cur)
PY
}

read_tool_version() {
  local file="$1"
  local tool="$2"
  awk -v tool="$tool" '$1 == tool { print $2; found=1; exit } END { if (!found) exit 1 }' "$file"
}

sanitize_ref() {
  printf '%s' "$1" | tr -c 'A-Za-z0-9_.=-' '-'
}
