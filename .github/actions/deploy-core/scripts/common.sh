#!/usr/bin/env bash
set -euo pipefail

# Sourced helper library for the deploy engine and adapter wrappers. Defines the
# shared shell helpers (annotations, output/env writers, input guards, lifecycle
# log and version-parse helpers, tar and cli-bin safety checks). It is never
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

# Fail with the SAME exit status the underlying tool returned.
#
# `fail` always exits 1, which erases a provider CLI's exit code. Callers that
# wrap a CLI must preserve it: an operator's `if: steps.x.outcome` logic and any
# retry tooling keys off the real status, and a rollback that exited 3 is not the
# same event as one that exited 1.
fail_with() {
  local code="$1"
  shift
  local message
  message=$(escape_annotation "$*")
  echo "::error::$message" >&2
  # Guard against a 0/blank status turning a failure into a silent success.
  [[ "$code" =~ ^[0-9]+$ ]] && ((code != 0)) || code=1
  exit "$code"
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

# The CLI binary name becomes a path component and is then chmod'd and executed,
# so it must be a bare filename — never a path, traversal, or dotfile.
validate_cli_bin() {
  local name="$1"
  [[ -n "$name" ]] || fail "CLI binary name must not be empty"
  case "$name" in
    */* | *\\* | *..*) fail "CLI binary name must not contain path separators or '..': '$name'" ;;
    .*) fail "CLI binary name must not start with '.': '$name'" ;;
  esac
  [[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] ||
    fail "CLI binary name may contain only letters, digits, '.', '_', '-': '$name'"
}

# Refuse an archive that could write outside the extraction directory: absolute
# paths, traversal, or symlink/hardlink members.
assert_safe_tarball() {
  local tarball="$1" member
  while IFS= read -r member; do
    case "$member" in
      /* | *..*) fail "refusing unsafe CLI archive member '$member'" ;;
    esac
  done < <(tar -tf "$tarball")
  # NOT `tar -tvf … | grep -q …`. Under `set -o pipefail`, grep -q exits on the
  # FIRST match, tar takes SIGPIPE, and the pipeline reports tar's failure — so
  # the `if` would be false exactly when a link WAS found, letting the unsafe
  # archive through. Capture first, then match.
  local listing
  listing=$(tar -tvf "$tarball")
  if grep -qE '^[lh]' <<<"$listing"; then
    fail "refusing CLI archive containing a symlink or hardlink member"
  fi
}

# ── Lifecycle helpers (deploy / healthcheck / rollback wrappers) ──────────────

# GitHub Actions does NOT enforce `required: true` on action inputs: an omitted
# or empty input is simply the empty string, and the step runs anyway. So the
# wrappers must check for themselves — otherwise an empty service-id or version
# silently reaches the provider.
require_input() {
  local name="$1" value="$2"
  [[ -n "$value" ]] || fail "missing required input '$name'"
}

require_input_matching() {
  local name="$1" value="$2" pattern="$3"
  require_input "$name" "$value"
  [[ "$value" =~ $pattern ]] || fail "input '$name' must match $pattern"
}

# The Fastly provider tooling and its pinned release binary are Linux x86-64
# only. Fail with a clear message rather than a confusing exec error later.
require_linux_x86_64() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) ;;
    *) fail "the Fastly wrapper supports only Linux x86-64 runners" ;;
  esac
}

# Create a private log file that is REMOVED when the caller exits, whatever the
# exit status. Provider CLIs print request URLs, service metadata, and — with
# debug flags — credential material; leaving a raw log behind in RUNNER_TEMP
# hands it to every later step in the job.
#
# Sets the global LIFECYCLE_LOG. Callers must have `set -euo pipefail`.
LIFECYCLE_LOG=""
new_private_log() {
  local dir="${RUNNER_TEMP:-/tmp}"
  LIFECYCLE_LOG=$(mktemp "$dir/edgezero-lifecycle.XXXXXX")
  chmod 600 "$LIFECYCLE_LOG"
  # shellcheck disable=SC2064  # expand LIFECYCLE_LOG now, not at trap time
  trap "rm -f -- '$LIFECYCLE_LOG'" EXIT
}

# Read a canonical `<key>=<digits>` line from a log.
#
# ANCHORED at both ends on purpose. An unanchored prefix match reads
# `version=15.2.0` as `15` and `version=12abc` as `12`, threading a version that
# was never deployed into the healthcheck and rollback that follow. If the value
# is not exactly digits, we have not parsed a version — we have guessed one.
read_numeric_line() {
  local key="$1" log="$2"
  grep -oE "^${key}=[0-9]+\$" "$log" | tail -n 1 | cut -d= -f2 || true
}

read_bool_line() {
  local key="$1" log="$2"
  grep -oE "^${key}=(true|false)\$" "$log" | tail -n 1 | cut -d= -f2 || true
}
