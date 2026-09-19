#!/usr/bin/env bash
set -euo pipefail

# Validates the application and Rust workspace selected by
# setup-rust-build-cache, binds its cache keys to the application and Cargo.lock
# bytes, and enables sccache for every later Rust build in the current GitHub
# Actions job. When target caching is enabled, it also publishes one confined,
# application-scoped CARGO_TARGET_DIR beneath RUNNER_TEMP.
#
# This script reads no application manifest and runs no application code. The
# action therefore remains independent of the package, adapter, and deployment
# that consume the compiled output.

fail() {
  printf '::error::%s\n' "$*" >&2
  exit 1
}

canonical_path() {
  local path="$1"
  realpath "$path" 2>/dev/null || fail "could not resolve path '$path'"
}

is_under() {
  local root="${1%/}"
  local path="${2%/}"
  [[ "$path" == "$root" || "$path" == "$root"/* ]]
}

sha256_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{ print $1 }'
  else
    fail "required command 'sha256sum' or 'shasum' was not found"
  fi
}

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local output_file="${GITHUB_OUTPUT:?GITHUB_OUTPUT is required}"
  local env_file="${GITHUB_ENV:?GITHUB_ENV is required}"
  local app_name="${EDGEZERO__RUST_CACHE__APP_NAME-}"
  local cache_target="${EDGEZERO__RUST_CACHE__CACHE_TARGET-false}"
  local working_directory="${EDGEZERO__RUST_CACHE__WORKING_DIRECTORY-}"

  [[ "$app_name" =~ ^[a-z0-9][a-z0-9._-]{0,63}$ ]] ||
    fail "input 'app-name' must be a lowercase app identifier using only letters, digits, '.', '_', or '-'"
  case "$cache_target" in
    true | false) ;;
    *) fail "input 'cache-target' must be 'true' or 'false'" ;;
  esac
  [[ -n "$working_directory" ]] ||
    fail "input 'working-directory' cannot be empty"
  [[ "$working_directory" != /* ]] ||
    fail "input 'working-directory' must be relative to github.workspace"
  command -v realpath >/dev/null 2>&1 ||
    fail "required command 'realpath' was not found"

  local workspace_real project_real lockfile
  workspace_real=$(canonical_path "$workspace")
  [[ -d "$workspace/$working_directory" ]] ||
    fail "working-directory '$working_directory' does not exist or is not a directory"
  project_real=$(canonical_path "$workspace/$working_directory")
  is_under "$workspace_real" "$project_real" ||
    fail "input 'working-directory' must resolve inside github.workspace"

  lockfile="$project_real/Cargo.lock"
  [[ -f "$lockfile" && ! -L "$lockfile" ]] ||
    fail "working-directory '$working_directory' must contain a regular Cargo.lock file"

  local lock_sha
  lock_sha=$(sha256_file "$lockfile")
  [[ "$lock_sha" =~ ^[0-9a-f]{64}$ ]] ||
    fail "could not compute a lowercase SHA-256 for Cargo.lock"

  {
    printf 'cache-namespace=%s\n' "$app_name"
    printf 'cargo-lock-sha256=%s\n' "$lock_sha"
  } >>"$output_file"
  {
    printf 'RUSTC_WRAPPER=sccache\n'
    printf 'SCCACHE_GHA_ENABLED=true\n'
    printf 'CARGO_INCREMENTAL=0\n'
  } >>"$env_file"

  if [[ "$cache_target" == "true" ]]; then
    local runner_temp="${RUNNER_TEMP:?RUNNER_TEMP is required when cache-target is true}"
    [[ -d "$runner_temp" ]] || fail "RUNNER_TEMP does not exist or is not a directory"

    local runner_temp_real target_dir
    runner_temp_real=$(canonical_path "$runner_temp")
    target_dir="$runner_temp_real/edgezero-rust-cache/$app_name/target"
    mkdir -p "$target_dir"
    target_dir=$(canonical_path "$target_dir")
    is_under "$runner_temp_real" "$target_dir" ||
      fail "Cargo target cache directory must resolve inside RUNNER_TEMP"

    printf 'cargo-target-dir=%s\n' "$target_dir" >>"$output_file"
    {
      printf 'CARGO_TARGET_DIR=%s\n' "$target_dir"
      printf 'EDGEZERO__RUST_CACHE__TARGET_DIR=%s\n' "$target_dir"
    } >>"$env_file"
  fi
}

main "$@"
