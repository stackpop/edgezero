#!/usr/bin/env bash
set -euo pipefail

# Validates the Rust workspace selected by setup-rust-build-cache, binds the
# Cargo dependency cache key to its Cargo.lock bytes, and enables sccache for
# every later Rust build in the current GitHub Actions job.
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
  local working_directory="${EDGEZERO__RUST_CACHE__WORKING_DIRECTORY-}"

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

  printf 'cargo-lock-sha256=%s\n' "$lock_sha" >>"$output_file"
  {
    printf 'RUSTC_WRAPPER=sccache\n'
    printf 'SCCACHE_GHA_ENABLED=true\n'
    printf 'CARGO_INCREMENTAL=0\n'
  } >>"$env_file"
}

main "$@"
