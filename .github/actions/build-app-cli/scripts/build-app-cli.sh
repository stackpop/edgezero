#!/usr/bin/env bash
set -euo pipefail

# Compiles the CLI package the APPLICATION provides — a crate in the app's own
# workspace — into an action-owned CARGO_TARGET_DIR, then packages the binary
# plus a self-describing app-cli-meta.json into a tar so the executable bit
# survives actions/upload-artifact. Never builds the EdgeZero monorepo CLI.
#
# Reads (env):
#   EDGEZERO__APP__CLI__PACKAGE           required  Cargo package name to build
#   EDGEZERO__ACTION__ROOT                required  EdgeZero action repo (toolchain fallback)
#   GITHUB_WORKSPACE                      required  checkout root; the search ceiling
#   EDGEZERO__APP__CLI__BIN               optional  binary name (default: the package name)
#   EDGEZERO__PROJECT__WORKING_DIRECTORY  optional  app dir under the workspace (default: ".")
#   EDGEZERO__PROJECT__RUST_TOOLCHAIN     optional  explicit toolchain or "auto" (default: "auto")
#   EDGEZERO__APP__CLI__ARTIFACT          optional  uploaded artifact name (default: "edgezero-cli")
#   RUNNER_TEMP                           optional  action-owned scratch root (default: /tmp)
# Writes (outputs):
#   app-cli-package                       the package that was built
#   app-cli-bin                           the binary name inside the artifact
#   app-cli-version                       version from cargo metadata
#   app-cli-artifact                      uploaded artifact name for downstream download
#   tarball-path                          absolute path of the staged tar

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

# --- Rust toolchain resolution helpers ---------------------------------------
parse_toolchain_from_channel_file() {
  local file="$1"
  # An extensionless `rust-toolchain` is EITHER a legacy single-line channel
  # (e.g. `1.95.0`) OR a TOML document (`[toolchain]` + `channel = "..."`).
  # rustup accepts both, so detect the TOML form and parse its channel rather
  # than taking the literal `[toolchain]` line as the channel.
  if grep -qE '^[[:space:]]*\[toolchain\]' "$file"; then
    parse_toolchain_from_toml "$file"
    return
  fi
  local value
  value=$(sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$file" | awk 'NF { print; exit }')
  [[ -n "$value" ]] || fail "malformed Rust toolchain file: $file"
  printf '%s\n' "$value"
}

parse_toolchain_from_toml() {
  local file="$1"
  local value
  value=$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*["'\''`]([^"'\''`]+)["'\''`][[:space:]]*(#.*)?$/\1/p' "$file" | head -n 1)
  [[ -n "$value" ]] || fail "malformed Rust toolchain TOML file: $file"
  printf '%s\n' "$value"
}

# The upward search for a toolchain file must not leave the APPLICATION's
# repository.
#
# The adoption guide's recommended layout checks the app out into a subdirectory
# of a separate deployer repo. Walking to github.workspace would then cross the
# app's Git boundary and let the *deployer's* `.tool-versions` silently decide
# which Rust compiles the app. The app's Git root is the honest boundary; fall
# back to github.workspace when the app dir is not a Git checkout at all.
#
# Every path here is canonicalized before comparison. The walk below tests the
# boundary with a string equality, so a symlinked TMPDIR or checkout would
# otherwise never match it — and the search would climb straight past the Git
# root it was meant to stop at.
resolve_search_boundary() {
  local app_dir="$1" workspace_root="$2"
  workspace_root=$(canonical_path "$workspace_root")

  local git_root
  git_root=$(git -C "$app_dir" rev-parse --show-toplevel 2>/dev/null) || {
    printf '%s\n' "$workspace_root"
    return
  }
  git_root=$(canonical_path "$git_root")

  # Never search above github.workspace, even if the app's Git root is higher
  # (a checkout mounted from outside the workspace).
  if is_under "$workspace_root" "$git_root"; then
    printf '%s\n' "$git_root"
  else
    printf '%s\n' "$workspace_root"
  fi
}

# Resolve the application toolchain: explicit input > rustup files (walking up to
# the app's Git root) > .tool-versions > the EdgeZero action repo fallback.
resolve_rust_toolchain() {
  local input="$1" app_dir="$2" workspace_root="$3" action_root="$4"
  if [[ "$input" != "auto" ]]; then
    [[ -n "$input" ]] || fail "input 'rust-toolchain' cannot be empty"
    printf '%s\n' "$input"
    return
  fi

  # Stop at the application repository boundary, not github.workspace.
  local boundary
  boundary=$(resolve_search_boundary "$app_dir" "$workspace_root")

  local directory value
  directory=$(canonical_path "$app_dir")
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
    if [[ "$directory" == "$boundary" ]]; then
      break
    fi
    local parent
    parent=$(dirname "$directory")
    if [[ "$parent" == "$directory" ]]; then
      break
    fi
    directory="$parent"
  done

  if [[ -f "$action_root/.tool-versions" ]] && value=$(read_tool_version "$action_root/.tool-versions" rust) && [[ -n "$value" ]]; then
    printf '%s\n' "$value"
    return
  fi
  fail "could not resolve Rust toolchain; checked rust-toolchain.toml, rust-toolchain, .tool-versions; set input 'rust-toolchain' explicitly"
}

require_linux_x86_64() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) ;;
    *) fail "build-app-cli supports only Linux x86-64 runners" ;;
  esac
}

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local action_root="${EDGEZERO__ACTION__ROOT:?EDGEZERO__ACTION__ROOT is required}"
  local cli_package="${EDGEZERO__APP__CLI__PACKAGE:?input 'app-cli-package' is required}"
  local cli_bin="${EDGEZERO__APP__CLI__BIN:-}"
  local working_directory="${EDGEZERO__PROJECT__WORKING_DIRECTORY:-.}"
  local rust_toolchain_input="${EDGEZERO__PROJECT__RUST_TOOLCHAIN:-auto}"
  local artifact_name="${EDGEZERO__APP__CLI__ARTIFACT:-edgezero-cli}"

  # These directories are `rm -rf`d below, so they must NEVER come from the
  # inherited environment — a colliding job-level variable could otherwise point
  # them at the checkout. Always derive them from the action-owned temp root.
  local runner_temp="${RUNNER_TEMP:-/tmp}"
  local stage_root="$runner_temp/edgezero-cli-artifact"
  local build_target_dir="$runner_temp/edgezero-cli-build"

  require_linux_x86_64
  require_cmd cargo
  require_cmd rustup
  require_cmd jq
  require_cmd tar
  validate_artifact_name "$artifact_name"

  # Resolve the application directory beneath github.workspace.
  local workspace_real app_dir
  workspace_real=$(canonical_path "$workspace")
  [[ -d "$workspace/$working_directory" ]] || fail "working-directory '$working_directory' does not exist or is not a directory"
  app_dir=$(canonical_path "$workspace/$working_directory")
  is_under "$workspace_real" "$app_dir" || fail "input 'working-directory' must resolve inside github.workspace"

  # Install the host toolchain only. The CLI is a native tool; the WASM target
  # the *application* needs is installed later by the deploy engine.
  local rust_toolchain
  rust_toolchain=$(resolve_rust_toolchain "$rust_toolchain_input" "$app_dir" "$workspace_real" "$action_root")
  rustup toolchain install "$rust_toolchain" --profile minimal

  # The application repository boundary — the same one the toolchain search stops
  # at. Falls back to github.workspace when the app dir is not a Git checkout.
  local app_boundary
  app_boundary=$(resolve_search_boundary "$app_dir" "$workspace_real")

  # Require a committed lockfile and validate the package + binary target.
  cd "$app_dir"
  local metadata package_json
  metadata=$(cargo +"$rust_toolchain" metadata --locked --no-deps --format-version 1) ||
    fail "cargo metadata --locked failed; ensure Cargo.lock is present and up to date"
  package_json=$(jq -c --arg p "$cli_package" '.packages[] | select(.name == $p)' <<<"$metadata")
  [[ -n "$package_json" ]] || fail "app-cli-package '$cli_package' was not found in the application workspace"

  # The Cargo WORKSPACE must live inside the application's repository.
  #
  # It is the workspace root that owns the lockfile and the resolved dependency
  # graph the artifact is built from. `cargo metadata` climbs to whatever
  # workspace encloses the app dir, so an enclosing deployer workspace would
  # otherwise control the deps of a CLI built from the app's source — the same
  # boundary violation as the package check below, but for the whole build.
  local workspace_root workspace_real
  workspace_root=$(jq -r '.workspace_root' <<<"$metadata")
  workspace_real=$(canonical_path "$workspace_root")
  is_under "$app_boundary" "$workspace_real" ||
    fail "the Cargo workspace root ($workspace_real) is outside the application at $app_boundary; an enclosing workspace would control the lockfile and dependencies the CLI is built from"

  # The package must live inside the APPLICATION's repository.
  #
  # `cargo metadata` resolves through whatever workspace encloses the app dir,
  # which in the separate-repository layout can be the DEPLOYER's workspace. The
  # app is supposed to own the CLI that deploys it, so a package resolved from
  # outside the app's Git root would compile code that `source-revision` never
  # describes. Bound it at the same boundary the toolchain search uses.
  local pkg_manifest pkg_real
  pkg_manifest=$(jq -r '.manifest_path' <<<"$package_json")
  pkg_real=$(canonical_path "$pkg_manifest")
  is_under "$app_boundary" "$pkg_real" ||
    fail "app-cli-package '$cli_package' resolves to $pkg_real, outside the application at $app_boundary; the app must own the CLI that deploys it"

  [[ -n "$cli_bin" ]] || cli_bin="$cli_package"
  local has_bin cli_version
  has_bin=$(jq -r --arg b "$cli_bin" '[.targets[] | select(.kind | index("bin")) | .name] | index($b) != null' <<<"$package_json")
  [[ "$has_bin" == "true" ]] || fail "app-cli-package '$cli_package' declares no binary target named '$cli_bin'"
  cli_version=$(jq -r '.version' <<<"$package_json")

  # Build into an action-owned target dir so the checkout stays clean.
  reset_owned_dir "$build_target_dir" "$runner_temp"
  CARGO_TARGET_DIR="$build_target_dir" cargo +"$rust_toolchain" build \
    --locked --release -p "$cli_package" --bin "$cli_bin"

  local bin_path="$build_target_dir/release/$cli_bin"
  [[ -x "$bin_path" ]] || fail "build did not produce an executable at $bin_path"
  "$bin_path" --help >/dev/null 2>&1 || fail "built CLI '$cli_bin' did not run '$cli_bin --help'"

  # Package the binary and self-describing metadata into a tar.
  reset_owned_dir "$stage_root" "$runner_temp"
  cp "$bin_path" "$stage_root/$cli_bin"
  chmod +x "$stage_root/$cli_bin"
  jq -n --arg bin "$cli_bin" --arg version "$cli_version" --arg package "$cli_package" \
    '{"app-cli-bin": $bin, "app-cli-version": $version, "app-cli-package": $package}' \
    >"$stage_root/app-cli-meta.json"

  # Fixed tarball name — never derive a path component from caller input.
  local tarball="$stage_root/../edgezero-cli.tar"
  tar -C "$stage_root" -cf "$tarball" "$cli_bin" app-cli-meta.json
  tarball=$(canonical_path "$tarball")

  notice "built app CLI '$cli_bin' v$cli_version from package '$cli_package'"
  append_output app-cli-version "$cli_version"
  append_output app-cli-package "$cli_package"
  append_output app-cli-bin "$cli_bin"
  append_output app-cli-artifact "$artifact_name"
  append_output tarball-path "$tarball"
}

# Sourcing this file exposes its functions without running a build, so the
# contract tests can exercise toolchain resolution directly.
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  main "$@"
fi
