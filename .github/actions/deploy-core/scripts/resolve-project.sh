#!/usr/bin/env bash
set -euo pipefail

# Resolves the application context for the deploy engine. Distinguishes the Git
# root (source revision + dirty-source guard) from the Cargo workspace root
# (Cargo.lock hash + target/ cache), so nested-workspace monorepos cache the
# right artifacts. Provider-neutral: no provider names appear here.
#
# Inputs (environment): EDGEZERO__INPUT__WORKING_DIRECTORY, EDGEZERO__INPUT__MANIFEST,
# EDGEZERO__INPUT__RUST_TOOLCHAIN, EDGEZERO__INPUT__TARGET (required), EDGEZERO__INPUT__BUILD_MODE, EDGEZERO__INPUT__CACHE,
# EDGEZERO__ACTION__ROOT (required), EDGEZERO__CLI__VERSION.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

# --- Rust toolchain resolution helpers ---------------------------------------
parse_toolchain_from_channel_file() {
  local value
  value=$(sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$1" | awk 'NF { print; exit }')
  [[ -n "$value" ]] || fail "malformed Rust toolchain file: $1"
  printf '%s\n' "$value"
}

parse_toolchain_from_toml() {
  local value
  value=$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*["'\''`]([^"'\''`]+)["'\''`][[:space:]]*$/\1/p' "$1" | head -n 1)
  [[ -n "$value" ]] || fail "malformed Rust toolchain TOML file: $1"
  printf '%s\n' "$value"
}

# Resolve the toolchain: explicit input > rustup files (walking to the Git root)
# > .tool-versions > the EdgeZero action repo fallback.
resolve_rust_toolchain() {
  local input="$1" app_dir="$2" git_root="$3" action_root="$4"
  if [[ "$input" != "auto" ]]; then
    [[ -n "$input" ]] || fail "input 'rust-toolchain' cannot be empty"
    printf '%s\n' "$input"
    return
  fi
  local directory="$app_dir" value
  while true; do
    if [[ -f "$directory/rust-toolchain.toml" ]]; then
      parse_toolchain_from_toml "$directory/rust-toolchain.toml"
      return
    fi
    if [[ -f "$directory/rust-toolchain" ]]; then
      parse_toolchain_from_channel_file "$directory/rust-toolchain"
      return
    fi
    if [[ -f "$directory/.tool-versions" ]] && value=$(read_tool_version "$directory/.tool-versions" rust) && [[ -n "$value" ]]; then
      printf '%s\n' "$value"
      return
    fi
    [[ "$directory" == "$git_root" ]] && break
    local parent
    parent=$(dirname "$directory")
    [[ "$parent" == "$directory" ]] && break
    directory="$parent"
  done
  if [[ -f "$action_root/.tool-versions" ]] && value=$(read_tool_version "$action_root/.tool-versions" rust) && [[ -n "$value" ]]; then
    printf '%s\n' "$value"
    return
  fi
  fail "could not resolve Rust toolchain; checked rust-toolchain.toml, rust-toolchain, .tool-versions; set input 'rust-toolchain' explicitly"
}

resolve_effective_build_mode() {
  case "${1:-auto}" in
    auto | never) printf 'never\n' ;;
    always) printf 'always\n' ;;
    *) fail "input 'build-mode' must be one of: auto, always, never" ;;
  esac
}

# Record the source revision and fail on a dirty working tree.
assert_committed_source() {
  local git_root="$1" app_rel="$2"
  if ! git -C "$git_root" diff --quiet --ignore-submodules -- ||
    ! git -C "$git_root" diff --cached --quiet --ignore-submodules -- ||
    [[ -n "$(git -C "$git_root" ls-files --others --exclude-standard)" ]]; then
    fail "deployments require committed source; working tree for '$app_rel' is dirty"
  fi
}

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local action_root="${EDGEZERO__ACTION__ROOT:?EDGEZERO__ACTION__ROOT is required}"
  local working_directory="${EDGEZERO__INPUT__WORKING_DIRECTORY:-.}"
  local manifest="${EDGEZERO__INPUT__MANIFEST:-}"
  local rust_toolchain_input="${EDGEZERO__INPUT__RUST_TOOLCHAIN:-auto}"
  local target="${EDGEZERO__INPUT__TARGET:?EDGEZERO__INPUT__TARGET is required (wrapper-provided concrete target)}"
  local cache="${EDGEZERO__INPUT__CACHE:-false}"
  local cli_version="${EDGEZERO__CLI__VERSION:-unknown}"

  require_cmd git
  require_cmd cargo

  # Application directory, confined to github.workspace.
  local workspace_real app_dir app_rel
  workspace_real=$(canonical_path "$workspace")
  [[ -d "$workspace/$working_directory" ]] || fail "working-directory '$working_directory' does not exist or is not a directory"
  app_dir=$(canonical_path "$workspace/$working_directory")
  is_under "$workspace_real" "$app_dir" || fail "input 'working-directory' must resolve inside github.workspace"
  app_rel=$(relative_to "$workspace_real" "$app_dir")

  # Optional explicit manifest.
  local manifest_path manifest_summary
  if [[ -n "$manifest" ]]; then
    [[ -f "$app_dir/$manifest" ]] || fail "manifest '$app_rel/$manifest' does not exist or is not a regular file"
    manifest_path=$(canonical_path "$app_dir/$manifest")
    is_under "$workspace_real" "$manifest_path" || fail "input 'manifest' must resolve inside github.workspace"
    manifest_summary=$(relative_to "$workspace_real" "$manifest_path")
  else
    manifest_path=""
    manifest_summary="EdgeZero default discovery"
  fi

  # Git root: source revision + dirty-source guard.
  local git_root source_revision
  git_root=$(git -C "$app_dir" rev-parse --show-toplevel 2>/dev/null || true)
  [[ -n "$git_root" ]] || fail "working-directory '$app_rel' is not inside a Git repository"
  git_root=$(canonical_path "$git_root")
  is_under "$workspace_real" "$git_root" || fail "application Git root must resolve inside github.workspace"
  source_revision=$(git -C "$git_root" rev-parse HEAD)
  assert_committed_source "$git_root" "$app_rel"

  # Cargo workspace root: lockfile + target/ + cache.
  local cargo_ws_manifest cargo_ws_root lockfile lock_hash target_dir
  if ! cargo_ws_manifest=$(cd "$app_dir" && cargo locate-project --workspace --message-format plain 2>/dev/null); then
    cargo_ws_manifest=""
  fi
  [[ -n "$cargo_ws_manifest" ]] || fail "could not locate the Cargo workspace root from '$app_rel'"
  cargo_ws_root=$(canonical_path "$(dirname "$cargo_ws_manifest")")
  lockfile="$cargo_ws_root/Cargo.lock"
  if [[ "$cache" == "true" && ! -f "$lockfile" ]]; then
    fail "cache is enabled but Cargo.lock was not found at the Cargo workspace root ($cargo_ws_root); exact-key caching requires Cargo.lock"
  fi
  lock_hash="none"
  if [[ -f "$lockfile" ]]; then
    lock_hash=$(sha256_file "$lockfile")
  fi
  target_dir="$cargo_ws_root/target"

  local rust_toolchain effective_build_mode cache_key
  rust_toolchain=$(resolve_rust_toolchain "$rust_toolchain_input" "$app_dir" "$git_root" "$action_root")
  effective_build_mode=$(resolve_effective_build_mode "${EDGEZERO__INPUT__BUILD_MODE:-auto}")
  cache_key="edgezero-deploy-${RUNNER_OS:-Linux}-${RUNNER_ARCH:-X64}-$(sanitize_ref "$rust_toolchain")-$(sanitize_ref "$target")-$(sanitize_ref "$cli_version")-${source_revision}-${lock_hash}"

  append_output working-directory "$app_dir"
  append_output working-directory-relative "$app_rel"
  append_output manifest "$manifest_path"
  append_output manifest-summary "$manifest_summary"
  append_output app-git-root "$git_root"
  append_output cargo-workspace-root "$cargo_ws_root"
  append_output source-revision "$source_revision"
  append_output rust-toolchain "$rust_toolchain"
  append_output effective-build-mode "$effective_build_mode"
  append_output cache-key "$cache_key"
  append_output cache-path "$target_dir"
}

main "$@"
