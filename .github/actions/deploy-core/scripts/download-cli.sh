#!/usr/bin/env bash
set -euo pipefail

# Extracts the build-cli artifact tar (downloaded by actions/download-artifact)
# into an action-owned tool dir, preserving the executable bit, reads the
# self-describing cli-meta.json, and prepends the dir to PATH for action steps.
# A wrapper-supplied INPUT_CLI_BIN overrides the metadata's binary name.
#
# Inputs (environment):
#   EDGEZERO_CLI_ARTIFACT_DIR  required  dir containing the downloaded tar
#   INPUT_CLI_BIN              optional  override for the binary name
#   EDGEZERO_TOOL_ROOT         optional  install dir (defaults under RUNNER_TEMP)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

# Locate the single CLI tar produced by build-cli within the downloaded artifact.
find_cli_tarball() {
  local artifact_dir="$1"
  find "$artifact_dir" -maxdepth 2 -type f -name '*.tar' | head -n 1
}

main() {
  local artifact_dir="${EDGEZERO_CLI_ARTIFACT_DIR:?EDGEZERO_CLI_ARTIFACT_DIR is required}"
  local cli_bin_override="${INPUT_CLI_BIN:-}"
  local tool_root="${EDGEZERO_TOOL_ROOT:-${RUNNER_TEMP:-/tmp}/edgezero-action-tools}"

  require_cmd jq
  require_cmd tar
  mkdir -p "$tool_root/bin"

  local tarball
  tarball=$(find_cli_tarball "$artifact_dir")
  [[ -n "$tarball" ]] || fail "no CLI tar found under the downloaded artifact at '$artifact_dir'"

  assert_safe_tarball "$tarball"
  tar -xf "$tarball" -C "$tool_root/bin"
  [[ -f "$tool_root/bin/cli-meta.json" ]] || fail "CLI artifact is missing cli-meta.json"

  local meta_bin cli_version cli_bin
  meta_bin=$(jq -er '."cli-bin"' "$tool_root/bin/cli-meta.json") || fail "cli-meta.json has no cli-bin"
  cli_version=$(jq -er '."cli-version"' "$tool_root/bin/cli-meta.json") || fail "cli-meta.json has no cli-version"
  cli_bin="${cli_bin_override:-$meta_bin}"
  validate_cli_bin "$cli_bin"

  local cli_path="$tool_root/bin/$cli_bin"
  [[ -f "$cli_path" && ! -L "$cli_path" ]] || fail "CLI binary '$cli_bin' is not a regular file in the artifact"
  chmod +x "$cli_path"

  # Smoke-check with a scrubbed environment: no inherited provider credential
  # (FASTLY_KEY, FASTLY_AUTH_TOKEN, ...) may reach the app CLI here.
  env -i PATH="/usr/bin:/bin" HOME="${HOME:-/tmp}" "$cli_path" --help >/dev/null 2>&1 ||
    fail "downloaded CLI '$cli_bin' did not run '--help'"

  printf '%s\n' "$tool_root/bin" >>"${GITHUB_PATH:-/dev/null}"
  export PATH="$tool_root/bin:$PATH"

  notice "using app CLI '$cli_bin' v$cli_version from artifact"
  append_output cli-bin "$cli_bin"
  append_output cli-version "$cli_version"
  append_env EDGEZERO_TOOL_ROOT "$tool_root"
}

main "$@"
