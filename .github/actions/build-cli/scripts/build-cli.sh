#!/usr/bin/env bash
set -euo pipefail

# Compiles the CLI package the *application* provides (a crate in the app's own
# workspace, named by INPUT_CLI_PACKAGE) into an action-owned CARGO_TARGET_DIR,
# then packages the binary plus a self-describing cli-meta.json into a tar so the
# executable bit survives actions/upload-artifact. Never builds the EdgeZero
# monorepo CLI.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

WORKSPACE=${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}
ACTION_ROOT=${EDGEZERO_ACTION_ROOT:?EDGEZERO_ACTION_ROOT is required}
CLI_PACKAGE=${INPUT_CLI_PACKAGE:?input 'cli-package' is required}
CLI_BIN=${INPUT_CLI_BIN:-}
WORKING_DIRECTORY=${INPUT_WORKING_DIRECTORY:-.}
RUST_TOOLCHAIN_INPUT=${INPUT_RUST_TOOLCHAIN:-auto}
ARTIFACT_NAME=${INPUT_ARTIFACT_NAME:-edgezero-cli}
STAGE_ROOT=${EDGEZERO_CLI_STAGE_ROOT:-${RUNNER_TEMP:-/tmp}/edgezero-cli-artifact}
BUILD_TARGET_DIR=${CARGO_TARGET_DIR_OVERRIDE:-${RUNNER_TEMP:-/tmp}/edgezero-cli-build}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64 | Linux-amd64) ;;
  *) fail "build-cli supports only Linux x86-64 runners" ;;
esac

require_cmd cargo
require_cmd rustup
require_cmd jq
require_cmd tar

# --- Resolve the application directory beneath github.workspace ---------------
WORKSPACE_REAL=$(canonical_path "$WORKSPACE")
APP_INPUT="$WORKSPACE/$WORKING_DIRECTORY"
[[ -d "$APP_INPUT" ]] || fail "working-directory '$WORKING_DIRECTORY' does not exist or is not a directory"
APP_DIR=$(canonical_path "$APP_INPUT")
is_under "$WORKSPACE_REAL" "$APP_DIR" || fail "input 'working-directory' must resolve inside github.workspace"

# --- Resolve the application Rust toolchain (input > rustup files > .tool-versions) --
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
    [[ "$directory" == "$WORKSPACE_REAL" ]] && break
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

# Install the host toolchain only. The CLI is a native tool; the WASM target the
# *application* needs is installed later by the deploy engine, not here.
rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal

# --- Validate the package + resolve the binary/version via cargo metadata -----
cd "$APP_DIR"
[[ -f Cargo.lock || -f "$(cargo locate-project --workspace --message-format plain 2>/dev/null | xargs -r dirname)/Cargo.lock" ]] ||
  fail "no Cargo.lock at the app's Cargo workspace root; build-cli requires a committed lockfile"

METADATA=$(cargo +"$RUST_TOOLCHAIN" metadata --locked --no-deps --format-version 1) ||
  fail "cargo metadata --locked failed; ensure Cargo.lock is present and up to date"

PKG_JSON=$(jq -c --arg p "$CLI_PACKAGE" '.packages[] | select(.name == $p)' <<<"$METADATA")
[[ -n "$PKG_JSON" ]] || fail "cli-package '$CLI_PACKAGE' was not found in the application workspace"

# Default the binary name to the package name; verify the bin target exists.
[[ -n "$CLI_BIN" ]] || CLI_BIN="$CLI_PACKAGE"
HAS_BIN=$(jq -r --arg b "$CLI_BIN" '[.targets[] | select(.kind | index("bin")) | .name] | index($b) != null' <<<"$PKG_JSON")
[[ "$HAS_BIN" == "true" ]] || fail "cli-package '$CLI_PACKAGE' declares no binary target named '$CLI_BIN'"
CLI_VERSION=$(jq -r '.version' <<<"$PKG_JSON")

# --- Build into an action-owned target dir (never dirties the checkout) -------
rm -rf "$BUILD_TARGET_DIR"
mkdir -p "$BUILD_TARGET_DIR"
CARGO_TARGET_DIR="$BUILD_TARGET_DIR" cargo +"$RUST_TOOLCHAIN" build \
  --locked --release -p "$CLI_PACKAGE" --bin "$CLI_BIN"

BIN_PATH="$BUILD_TARGET_DIR/release/$CLI_BIN"
[[ -x "$BIN_PATH" ]] || fail "build did not produce an executable at $BIN_PATH"

# Smoke-check runnability (today's generated CLI may have no --version).
"$BIN_PATH" --help >/dev/null 2>&1 || fail "built CLI '$CLI_BIN' did not run '$CLI_BIN --help'"

# --- Package binary + self-describing metadata into a tar ---------------------
rm -rf "$STAGE_ROOT"
mkdir -p "$STAGE_ROOT"
cp "$BIN_PATH" "$STAGE_ROOT/$CLI_BIN"
chmod +x "$STAGE_ROOT/$CLI_BIN"
jq -n --arg bin "$CLI_BIN" --arg version "$CLI_VERSION" --arg package "$CLI_PACKAGE" \
  '{"cli-bin": $bin, "cli-version": $version, "cli-package": $package}' \
  >"$STAGE_ROOT/cli-meta.json"

TARBALL="$STAGE_ROOT/../${ARTIFACT_NAME}.tar"
tar -C "$STAGE_ROOT" -cf "$TARBALL" "$CLI_BIN" cli-meta.json
TARBALL=$(canonical_path "$TARBALL")

notice "built app CLI '$CLI_BIN' v$CLI_VERSION from package '$CLI_PACKAGE'"
append_output cli-version "$CLI_VERSION"
append_output cli-package "$CLI_PACKAGE"
append_output cli-bin "$CLI_BIN"
append_output artifact-name "$ARTIFACT_NAME"
append_output tarball-path "$TARBALL"
