#!/usr/bin/env bash
set -euo pipefail

# Compiles the CLI package the *application* provides (a crate in the app's own
# workspace, named by INPUT_CLI_PACKAGE) into an action-owned CARGO_TARGET_DIR,
# then packages the binary plus a self-describing cli-meta.json into a tar so the
# executable bit survives actions/upload-artifact. Never builds the EdgeZero
# monorepo CLI.
#
# Inputs (environment):
#   INPUT_CLI_PACKAGE       required  Cargo package name to build
#   INPUT_CLI_BIN           optional  binary name (defaults to the package name)
#   INPUT_WORKING_DIRECTORY optional  app dir relative to github.workspace (".")
#   INPUT_RUST_TOOLCHAIN    optional  explicit toolchain or "auto"
#   INPUT_ARTIFACT_NAME     optional  uploaded artifact name ("edgezero-cli")

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

# --- Rust toolchain resolution helpers ---------------------------------------
parse_toolchain_from_channel_file() {
  local file="$1"
  local value
  value=$(sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$file" | awk 'NF { print; exit }')
  [[ -n "$value" ]] || fail "malformed Rust toolchain file: $file"
  printf '%s\n' "$value"
}

parse_toolchain_from_toml() {
  local file="$1"
  local value
  value=$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*["'\''`]([^"'\''`]+)["'\''`][[:space:]]*$/\1/p' "$file" | head -n 1)
  [[ -n "$value" ]] || fail "malformed Rust toolchain TOML file: $file"
  printf '%s\n' "$value"
}

# Resolve the application toolchain: explicit input > rustup files (walking up to
# github.workspace) > .tool-versions > the EdgeZero action repo fallback.
resolve_rust_toolchain() {
  local input="$1" app_dir="$2" workspace_root="$3" action_root="$4"
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
    [[ "$directory" == "$workspace_root" ]] && break
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

require_linux_x86_64() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) ;;
    *) fail "build-cli supports only Linux x86-64 runners" ;;
  esac
}

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local action_root="${EDGEZERO_ACTION_ROOT:?EDGEZERO_ACTION_ROOT is required}"
  local cli_package="${INPUT_CLI_PACKAGE:?input 'cli-package' is required}"
  local cli_bin="${INPUT_CLI_BIN:-}"
  local working_directory="${INPUT_WORKING_DIRECTORY:-.}"
  local rust_toolchain_input="${INPUT_RUST_TOOLCHAIN:-auto}"
  local artifact_name="${INPUT_ARTIFACT_NAME:-edgezero-cli}"
  local stage_root="${EDGEZERO_CLI_STAGE_ROOT:-${RUNNER_TEMP:-/tmp}/edgezero-cli-artifact}"
  local build_target_dir="${CARGO_TARGET_DIR_OVERRIDE:-${RUNNER_TEMP:-/tmp}/edgezero-cli-build}"

  require_linux_x86_64
  require_cmd cargo
  require_cmd rustup
  require_cmd jq
  require_cmd tar

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

  # Require a committed lockfile and validate the package + binary target.
  cd "$app_dir"
  local metadata package_json
  metadata=$(cargo +"$rust_toolchain" metadata --locked --no-deps --format-version 1) ||
    fail "cargo metadata --locked failed; ensure Cargo.lock is present and up to date"
  package_json=$(jq -c --arg p "$cli_package" '.packages[] | select(.name == $p)' <<<"$metadata")
  [[ -n "$package_json" ]] || fail "cli-package '$cli_package' was not found in the application workspace"

  [[ -n "$cli_bin" ]] || cli_bin="$cli_package"
  local has_bin cli_version
  has_bin=$(jq -r --arg b "$cli_bin" '[.targets[] | select(.kind | index("bin")) | .name] | index($b) != null' <<<"$package_json")
  [[ "$has_bin" == "true" ]] || fail "cli-package '$cli_package' declares no binary target named '$cli_bin'"
  cli_version=$(jq -r '.version' <<<"$package_json")

  # Build into an action-owned target dir so the checkout stays clean.
  rm -rf "$build_target_dir"
  mkdir -p "$build_target_dir"
  CARGO_TARGET_DIR="$build_target_dir" cargo +"$rust_toolchain" build \
    --locked --release -p "$cli_package" --bin "$cli_bin"

  local bin_path="$build_target_dir/release/$cli_bin"
  [[ -x "$bin_path" ]] || fail "build did not produce an executable at $bin_path"
  "$bin_path" --help >/dev/null 2>&1 || fail "built CLI '$cli_bin' did not run '$cli_bin --help'"

  # Package the binary and self-describing metadata into a tar.
  rm -rf "$stage_root"
  mkdir -p "$stage_root"
  cp "$bin_path" "$stage_root/$cli_bin"
  chmod +x "$stage_root/$cli_bin"
  jq -n --arg bin "$cli_bin" --arg version "$cli_version" --arg package "$cli_package" \
    '{"cli-bin": $bin, "cli-version": $version, "cli-package": $package}' \
    >"$stage_root/cli-meta.json"

  local tarball="$stage_root/../${artifact_name}.tar"
  tar -C "$stage_root" -cf "$tarball" "$cli_bin" cli-meta.json
  tarball=$(canonical_path "$tarball")

  notice "built app CLI '$cli_bin' v$cli_version from package '$cli_package'"
  append_output cli-version "$cli_version"
  append_output cli-package "$cli_package"
  append_output cli-bin "$cli_bin"
  append_output artifact-name "$artifact_name"
  append_output tarball-path "$tarball"
}

main "$@"
