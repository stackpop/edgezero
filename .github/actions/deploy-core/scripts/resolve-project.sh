#!/usr/bin/env bash
set -euo pipefail

# Resolves the application context for the deploy engine. Distinguishes the Git
# root (source revision + dirty-source guard) from the Cargo workspace root
# (Cargo.lock hash + target/ cache), so nested-workspace monorepos cache the
# right artifacts. Provider-neutral: no provider names appear here.
#
# Reads (env):
#   EDGEZERO__PROJECT__TARGET             required  concrete build target (e.g. wasm32-wasip1)
#   EDGEZERO__ACTION__ROOT                required  EdgeZero action repo (toolchain fallback)
#   GITHUB_WORKSPACE                      required  checkout root; the search ceiling
#   EDGEZERO__PROJECT__WORKING_DIRECTORY  optional  app dir under the workspace (default: ".")
#   EDGEZERO__PROJECT__MANIFEST           optional  edgezero.toml path, relative to the app dir
#   EDGEZERO__PROJECT__RUST_TOOLCHAIN     optional  explicit toolchain or "auto" (default: "auto")
#   EDGEZERO__BUILD__MODE                 optional  auto | always | never (default: auto)
#   EDGEZERO__BUILD__CACHE                optional  true | false (default: false)
#   EDGEZERO__APP__CLI__VERSION           optional  folded into the cache key (default: unknown)
# Writes (outputs):
#   working-directory                     resolved absolute app directory
#   working-directory-relative            app directory relative to the workspace
#   app-git-root                          enclosing Git repository (source revision)
#   cargo-workspace-root                  Cargo workspace root (Cargo.lock + target/)
#   source-revision                       Git revision of the app
#   manifest / manifest-summary           resolved manifest path (and a display form)
#   rust-toolchain                        resolved toolchain
#   effective-build-mode                  build mode after auto resolution
#   cache-key / cache-path                exact cache key and the target/ path it covers

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
  local working_directory="${EDGEZERO__PROJECT__WORKING_DIRECTORY:-.}"
  local manifest="${EDGEZERO__PROJECT__MANIFEST:-}"
  local rust_toolchain_input="${EDGEZERO__PROJECT__RUST_TOOLCHAIN:-auto}"
  local target="${EDGEZERO__PROJECT__TARGET:?EDGEZERO__PROJECT__TARGET is required (wrapper-provided concrete target)}"
  local cache="${EDGEZERO__BUILD__CACHE:-false}"
  local cli_version="${EDGEZERO__APP__CLI__VERSION:-unknown}"

  require_cmd git
  require_cmd cargo

  # Application directory, confined to github.workspace.
  local workspace_real app_dir app_rel
  workspace_real=$(canonical_path "$workspace")
  [[ -d "$workspace/$working_directory" ]] || fail "working-directory '$working_directory' does not exist or is not a directory"
  app_dir=$(canonical_path "$workspace/$working_directory")
  is_under "$workspace_real" "$app_dir" || fail "input 'working-directory' must resolve inside github.workspace"
  app_rel=$(relative_to "$workspace_real" "$app_dir")

  # Git root: source revision + dirty-source guard. Resolved BEFORE the manifest
  # so it can bound it — github.workspace is too loose a boundary. In the
  # separate-repository layout the deployer repo IS the workspace, so a manifest
  # like `../deployer/edgezero.toml` would be "inside github.workspace" yet come
  # from a different repository than the source-revision we report.
  local git_root source_revision
  git_root=$(git -C "$app_dir" rev-parse --show-toplevel 2>/dev/null || true)
  [[ -n "$git_root" ]] || fail "working-directory '$app_rel' is not inside a Git repository"
  git_root=$(canonical_path "$git_root")
  is_under "$workspace_real" "$git_root" || fail "application Git root must resolve inside github.workspace"

  # Optional explicit manifest, confined to the application repository.
  local manifest_path manifest_summary
  if [[ -n "$manifest" ]]; then
    [[ -f "$app_dir/$manifest" ]] || fail "manifest '$app_rel/$manifest' does not exist or is not a regular file"
    manifest_path=$(canonical_path "$app_dir/$manifest")
    is_under "$git_root" "$manifest_path" ||
      fail "input 'manifest' must resolve inside the application repository ($git_root); '$manifest' escapes it"
    manifest_summary=$(relative_to "$workspace_real" "$manifest_path")
  else
    manifest_path=""
    manifest_summary="EdgeZero default discovery"
    # Default discovery is NOT unconfined. The CLI runs from the app dir and
    # defaults to `edgezero.toml` there; a committed symlink named `edgezero.toml`
    # could point outside the app repository, and its manifest deploy command
    # would run with provider credentials. If that file exists, it must resolve
    # inside the app repository — the same rule as an explicit manifest.
    if [[ -e "$app_dir/edgezero.toml" ]]; then
      local default_manifest
      default_manifest=$(canonical_path "$app_dir/edgezero.toml")
      is_under "$git_root" "$default_manifest" ||
        fail "the default 'edgezero.toml' in '$app_rel' resolves outside the application repository ($git_root) — refusing to run a manifest that escapes it"
    fi
  fi

  # Source revision + dirty-source guard (git_root resolved above).
  local source_revision
  source_revision=$(git -C "$git_root" rev-parse HEAD)
  assert_committed_source "$git_root" "$app_rel"

  # Cargo workspace root: lockfile + target/ + cache.
  #
  # It may legitimately sit ABOVE the app dir (a monorepo app in `apps/api/`
  # keyed on the repo-root workspace), but never above the app's REPOSITORY:
  # `cargo locate-project --workspace` climbs until it finds a workspace root, so
  # in the separate-repository layout it can escape into the deployer's workspace
  # and we would build and cache code that `source-revision` does not describe.
  local cargo_ws_manifest cargo_ws_root lockfile lock_hash target_dir
  if ! cargo_ws_manifest=$(cd "$app_dir" && cargo locate-project --workspace --message-format plain 2>/dev/null); then
    cargo_ws_manifest=""
  fi
  [[ -n "$cargo_ws_manifest" ]] || fail "could not locate the Cargo workspace root from '$app_rel'"
  cargo_ws_root=$(canonical_path "$(dirname "$cargo_ws_manifest")")
  is_under "$git_root" "$cargo_ws_root" ||
    fail "the Cargo workspace root ($cargo_ws_root) is outside the application repository ($git_root); refusing to build code that 'source-revision' does not describe"
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
  effective_build_mode=$(resolve_effective_build_mode "${EDGEZERO__BUILD__MODE:-auto}")
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

# Sourcing this file exposes its functions without resolving a project, so the
# contract tests can exercise the dirty-source guard directly.
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  main "$@"
fi
