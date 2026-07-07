#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

WORKSPACE=${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}
ACTION_ROOT=${EDGEZERO_ACTION_ROOT:?EDGEZERO_ACTION_ROOT is required}
WORKING_DIRECTORY=${INPUT_WORKING_DIRECTORY:-.}
MANIFEST=${INPUT_MANIFEST:-}
RUST_TOOLCHAIN_INPUT=${INPUT_RUST_TOOLCHAIN:-auto}
CACHE=${INPUT_CACHE:-false}
ACTION_REF=${EDGEZERO_ACTION_REF:-}

WORKSPACE_REAL=$(canonical_path "$WORKSPACE")
APP_INPUT="$WORKSPACE/$WORKING_DIRECTORY"
[[ -d "$APP_INPUT" ]] || fail "working-directory '$WORKING_DIRECTORY' does not exist or is not a directory"
APP_DIR=$(canonical_path "$APP_INPUT")
is_under "$WORKSPACE_REAL" "$APP_DIR" || fail "input 'working-directory' must resolve inside github.workspace"
APP_REL=$(relative_to "$WORKSPACE_REAL" "$APP_DIR")

if [[ -n "$MANIFEST" ]]; then
  MANIFEST_INPUT="$APP_DIR/$MANIFEST"
  [[ -f "$MANIFEST_INPUT" ]] || fail "manifest '$APP_REL/$MANIFEST' does not exist or is not a regular file"
  MANIFEST_PATH=$(canonical_path "$MANIFEST_INPUT")
  is_under "$WORKSPACE_REAL" "$MANIFEST_PATH" || fail "input 'manifest' must resolve inside github.workspace"
  MANIFEST_REL=$(relative_to "$WORKSPACE_REAL" "$MANIFEST_PATH")
else
  MANIFEST_PATH=""
  MANIFEST_REL="EdgeZero default discovery"
fi

APP_GIT_ROOT=$(git -C "$APP_DIR" rev-parse --show-toplevel 2>/dev/null || true)
[[ -n "$APP_GIT_ROOT" ]] || fail "working-directory '$APP_REL' is not inside a Git repository"
APP_GIT_ROOT=$(canonical_path "$APP_GIT_ROOT")
is_under "$WORKSPACE_REAL" "$APP_GIT_ROOT" || fail "application Git root must resolve inside github.workspace"
SOURCE_REVISION=$(git -C "$APP_GIT_ROOT" rev-parse HEAD)
if ! git -C "$APP_GIT_ROOT" diff --quiet --ignore-submodules -- || \
   ! git -C "$APP_GIT_ROOT" diff --cached --quiet --ignore-submodules -- || \
   [[ -n "$(git -C "$APP_GIT_ROOT" ls-files --others --exclude-standard)" ]]; then
  fail "deployments require committed source; working tree for '$APP_REL' is dirty"
fi

parse_rust_toolchain_file() {
  local file="$1"
  local value
  value=$(sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$file" | awk 'NF { print; exit }')
  [[ -n "$value" ]] || fail "malformed Rust toolchain file: $file"
  printf '%s\n' "$value"
}

parse_rust_toolchain_toml() {
  local file="$1"
  local value
  value=$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*["'\''`]([^"'\''`]+)["'\''`][[:space:]]*$/\1/p' "$file" | head -n 1)
  [[ -n "$value" ]] || fail "malformed Rust toolchain TOML file: $file"
  printf '%s\n' "$value"
}

parse_rust_toolchain_from_tool_versions() {
  local file="$1"
  awk '$1 == "rust" { found=1; if (NF < 2) exit 2; print $2; exit } END { if (!found) exit 1 }' "$file"
}

resolve_rust_toolchain() {
  if [[ "$RUST_TOOLCHAIN_INPUT" != "auto" ]]; then
    [[ -n "$RUST_TOOLCHAIN_INPUT" ]] || fail "input 'rust-toolchain' cannot be empty"
    printf '%s\n' "$RUST_TOOLCHAIN_INPUT"
    return
  fi

  local directory="$APP_DIR"
  local value status
  while true; do
    if [[ -f "$directory/rust-toolchain.toml" ]]; then
      parse_rust_toolchain_toml "$directory/rust-toolchain.toml"
      return
    fi
    if [[ -f "$directory/rust-toolchain" ]]; then
      parse_rust_toolchain_file "$directory/rust-toolchain"
      return
    fi
    if [[ -f "$directory/.tool-versions" ]]; then
      set +e
      value=$(parse_rust_toolchain_from_tool_versions "$directory/.tool-versions")
      status=$?
      set -e
      case "$status" in
        0) printf '%s\n' "$value"; return ;;
        1) ;;
        2) fail "malformed .tool-versions rust entry: $directory/.tool-versions" ;;
        *) fail "could not parse .tool-versions rust entry: $directory/.tool-versions" ;;
      esac
    fi
    [[ "$directory" == "$APP_GIT_ROOT" ]] && break
    local next
    next=$(dirname "$directory")
    [[ "$next" == "$directory" ]] && break
    directory="$next"
  done

  if [[ -f "$ACTION_ROOT/.tool-versions" ]]; then
    set +e
    value=$(parse_rust_toolchain_from_tool_versions "$ACTION_ROOT/.tool-versions")
    status=$?
    set -e
    case "$status" in
      0) printf '%s\n' "$value"; return ;;
      1) ;;
      2) fail "malformed .tool-versions rust entry: $ACTION_ROOT/.tool-versions" ;;
      *) fail "could not parse .tool-versions rust entry: $ACTION_ROOT/.tool-versions" ;;
    esac
  fi
  fail "could not resolve Rust toolchain; checked rust-toolchain.toml, rust-toolchain, and .tool-versions; set input rust-toolchain explicitly"
}

RUST_TOOLCHAIN=$(resolve_rust_toolchain)
ACTION_RUST_TOOLCHAIN=$(read_tool_version "$ACTION_ROOT/.tool-versions" rust || true)
[[ -n "$ACTION_RUST_TOOLCHAIN" ]] || fail "EdgeZero repository .tool-versions must contain a rust entry"

LOCKFILE="$APP_GIT_ROOT/Cargo.lock"
if [[ "$CACHE" == "true" && ! -f "$LOCKFILE" ]]; then
  fail "cache is enabled but Cargo.lock was not found at application Git root; exact-key caching requires Cargo.lock"
fi
LOCK_HASH="none"
if [[ -f "$LOCKFILE" ]]; then
  LOCK_HASH=$(sha256_file "$LOCKFILE")
fi

if EDGEZERO_REVISION=$(git -C "$ACTION_ROOT" rev-parse HEAD 2>/dev/null); then
  :
elif [[ "$ACTION_REF" =~ ^[0-9a-fA-F]{40}$ ]]; then
  EDGEZERO_REVISION=$ACTION_REF
else
  fail "could not resolve immutable EdgeZero action revision; pin the action with a full commit SHA"
fi
CACHE_KEY="edgezero-deploy-${RUNNER_OS:-Linux}-${RUNNER_ARCH:-X64}-$(sanitize_ref "$RUST_TOOLCHAIN")-wasm32-wasip1-${EDGEZERO_REVISION}-${SOURCE_REVISION}-${LOCK_HASH}"
TARGET_DIR="$APP_GIT_ROOT/target"

effective_build_mode() {
  case "${INPUT_BUILD_MODE:-auto}" in
    auto) printf 'never\n' ;;
    always) printf 'always\n' ;;
    never) printf 'never\n' ;;
    *) fail "input 'build-mode' must be one of: auto, always, never" ;;
  esac
}
EFFECTIVE_BUILD_MODE=$(effective_build_mode)

append_output working-directory "$APP_DIR"
append_output working-directory-relative "$APP_REL"
append_output manifest "$MANIFEST_PATH"
append_output manifest-summary "$MANIFEST_REL"
append_output app-git-root "$APP_GIT_ROOT"
append_output source-revision "$SOURCE_REVISION"
append_output rust-toolchain "$RUST_TOOLCHAIN"
append_output action-rust-toolchain "$ACTION_RUST_TOOLCHAIN"
append_output edgezero-revision "$EDGEZERO_REVISION"
append_output effective-build-mode "$EFFECTIVE_BUILD_MODE"
append_output cache-key "$CACHE_KEY"
append_output cache-path "$TARGET_DIR"
