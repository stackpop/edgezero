#!/usr/bin/env bash
set -euo pipefail

# Resolves the application context for the deploy engine. Distinguishes the Git
# root (source revision + dirty-source guard) from the Cargo workspace root
# (Cargo.lock hash + target/ cache), so nested-workspace monorepos cache the
# right artifacts. Provider-neutral: no provider names appear here.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

WORKSPACE=${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}
ACTION_ROOT=${EDGEZERO_ACTION_ROOT:?EDGEZERO_ACTION_ROOT is required}
WORKING_DIRECTORY=${INPUT_WORKING_DIRECTORY:-.}
MANIFEST=${INPUT_MANIFEST:-}
RUST_TOOLCHAIN_INPUT=${INPUT_RUST_TOOLCHAIN:-auto}
RUST_TARGET=${INPUT_TARGET:?INPUT_TARGET is required (wrapper-provided concrete target)}
CACHE=${INPUT_CACHE:-false}
CLI_VERSION=${EDGEZERO_CLI_VERSION:-unknown}

require_cmd git
require_cmd cargo

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

# --- Git root: source revision + dirty-source guard ---------------------------
APP_GIT_ROOT=$(git -C "$APP_DIR" rev-parse --show-toplevel 2>/dev/null || true)
[[ -n "$APP_GIT_ROOT" ]] || fail "working-directory '$APP_REL' is not inside a Git repository"
APP_GIT_ROOT=$(canonical_path "$APP_GIT_ROOT")
is_under "$WORKSPACE_REAL" "$APP_GIT_ROOT" || fail "application Git root must resolve inside github.workspace"
SOURCE_REVISION=$(git -C "$APP_GIT_ROOT" rev-parse HEAD)
if ! git -C "$APP_GIT_ROOT" diff --quiet --ignore-submodules -- ||
  ! git -C "$APP_GIT_ROOT" diff --cached --quiet --ignore-submodules -- ||
  [[ -n "$(git -C "$APP_GIT_ROOT" ls-files --others --exclude-standard)" ]]; then
  fail "deployments require committed source; working tree for '$APP_REL' is dirty"
fi

# --- Cargo workspace root: lockfile + target/ + cache -------------------------
if ! WORKSPACE_MANIFEST=$(cd "$APP_DIR" && cargo locate-project --workspace --message-format plain 2>/dev/null); then
  WORKSPACE_MANIFEST=""
fi
[[ -n "$WORKSPACE_MANIFEST" ]] || fail "could not locate the Cargo workspace root from '$APP_REL'"
CARGO_WS_ROOT=$(canonical_path "$(dirname "$WORKSPACE_MANIFEST")")
LOCKFILE="$CARGO_WS_ROOT/Cargo.lock"
if [[ "$CACHE" == "true" && ! -f "$LOCKFILE" ]]; then
  fail "cache is enabled but Cargo.lock was not found at the Cargo workspace root ($CARGO_WS_ROOT); exact-key caching requires Cargo.lock"
fi
LOCK_HASH="none"
[[ -f "$LOCKFILE" ]] && LOCK_HASH=$(sha256_file "$LOCKFILE")
TARGET_DIR="$CARGO_WS_ROOT/target"

# --- Rust toolchain resolution (input > rustup files > .tool-versions) --------
parse_rust_toolchain_file() {
  local value
  value=$(sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$1" | awk 'NF { print; exit }')
  [[ -n "$value" ]] || fail "malformed Rust toolchain file: $1"
  printf '%s\n' "$value"
}
parse_rust_toolchain_toml() {
  local value
  value=$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*["'\''`]([^"'\''`]+)["'\''`][[:space:]]*$/\1/p' "$1" | head -n 1)
  [[ -n "$value" ]] || fail "malformed Rust toolchain TOML file: $1"
  printf '%s\n' "$value"
}
resolve_rust_toolchain() {
  if [[ "$RUST_TOOLCHAIN_INPUT" != "auto" ]]; then
    [[ -n "$RUST_TOOLCHAIN_INPUT" ]] || fail "input 'rust-toolchain' cannot be empty"
    printf '%s\n' "$RUST_TOOLCHAIN_INPUT"
    return
  fi
  local directory="$APP_DIR" value
  while true; do
    if [[ -f "$directory/rust-toolchain.toml" ]]; then
      parse_rust_toolchain_toml "$directory/rust-toolchain.toml"
      return
    fi
    if [[ -f "$directory/rust-toolchain" ]]; then
      parse_rust_toolchain_file "$directory/rust-toolchain"
      return
    fi
    if [[ -f "$directory/.tool-versions" ]] && value=$(read_tool_version "$directory/.tool-versions" rust) && [[ -n "$value" ]]; then
      printf '%s\n' "$value"
      return
    fi
    [[ "$directory" == "$APP_GIT_ROOT" ]] && break
    local next
    next=$(dirname "$directory")
    [[ "$next" == "$directory" ]] && break
    directory="$next"
  done
  if [[ -f "$ACTION_ROOT/.tool-versions" ]] && value=$(read_tool_version "$ACTION_ROOT/.tool-versions" rust) && [[ -n "$value" ]]; then
    printf '%s\n' "$value"
    return
  fi
  fail "could not resolve Rust toolchain; checked rust-toolchain.toml, rust-toolchain, .tool-versions; set input 'rust-toolchain' explicitly"
}
RUST_TOOLCHAIN=$(resolve_rust_toolchain)

CACHE_KEY="edgezero-deploy-${RUNNER_OS:-Linux}-${RUNNER_ARCH:-X64}-$(sanitize_ref "$RUST_TOOLCHAIN")-$(sanitize_ref "$RUST_TARGET")-$(sanitize_ref "$CLI_VERSION")-${SOURCE_REVISION}-${LOCK_HASH}"

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
append_output cargo-workspace-root "$CARGO_WS_ROOT"
append_output source-revision "$SOURCE_REVISION"
append_output rust-toolchain "$RUST_TOOLCHAIN"
append_output effective-build-mode "$EFFECTIVE_BUILD_MODE"
append_output cache-key "$CACHE_KEY"
append_output cache-path "$TARGET_DIR"
