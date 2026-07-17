#!/usr/bin/env bash
set -euo pipefail

# Extracts the build-app-cli artifact tar (downloaded by actions/download-artifact)
# into an action-owned tool dir, preserving the executable bit, reads the
# self-describing app-cli-meta.json, and prepends the dir to PATH for action steps.
# A wrapper-supplied EDGEZERO__APP__CLI__BIN overrides the metadata's binary name.
#
# Reads (env):
#   EDGEZERO__APP__CLI__ARTIFACT_DIR      required  dir containing the downloaded tar
#   EDGEZERO__APP__CLI__BIN               optional  override for the binary name
#   EDGEZERO__ACTION__TOOL_ROOT           optional  install dir (default: under RUNNER_TEMP)
# Writes (outputs):
#   app-cli-bin                           resolved binary name
#   app-cli-version                       version from app-cli-meta.json
# Writes (env):
#   EDGEZERO__ACTION__TOOL_ROOT           the install dir (for later steps + cleanup)
# Writes (PATH):
#   the install dir's bin/, so the app CLI is callable by name

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

# Locate the single CLI tar produced by build-app-cli within the downloaded
# artifact.
#
# Refuses to guess. `actions/download-artifact` with an empty `name:` downloads
# EVERY artifact in the run, so silently taking the first tar could execute an
# unrelated binary — with provider credentials in scope. Exactly one tar is the
# only unambiguous case; zero or many is a caller error, not a coin toss.
find_cli_tarball() {
  local artifact_dir="$1"
  local -a tarballs=()
  while IFS= read -r found; do
    tarballs+=("$found")
  done < <(find "$artifact_dir" -maxdepth 2 -type f -name '*.tar' | sort)

  case "${#tarballs[@]}" in
    1) printf '%s\n' "${tarballs[0]}" ;;
    0) fail "no CLI tar found in the downloaded artifact ($artifact_dir); check the 'app-cli-artifact' input names a build-app-cli artifact" ;;
    *) fail "expected exactly 1 CLI tar in the downloaded artifact, found ${#tarballs[@]} — refusing to guess which CLI to run: ${tarballs[*]}" ;;
  esac
}

main() {
  local artifact_dir="${EDGEZERO__APP__CLI__ARTIFACT_DIR:?EDGEZERO__APP__CLI__ARTIFACT_DIR is required}"
  local cli_bin_override="${EDGEZERO__APP__CLI__BIN:-}"
  local tool_root="${EDGEZERO__ACTION__TOOL_ROOT:-${RUNNER_TEMP:-/tmp}/edgezero-action-tools}"

  require_cmd jq
  require_cmd tar
  mkdir -p "$tool_root/bin"

  local tarball
  tarball=$(find_cli_tarball "$artifact_dir")
  [[ -n "$tarball" ]] || fail "no CLI tar found under the downloaded artifact at '$artifact_dir'"

  assert_safe_tarball "$tarball"
  tar -xf "$tarball" -C "$tool_root/bin"
  [[ -f "$tool_root/bin/app-cli-meta.json" ]] || fail "CLI artifact is missing app-cli-meta.json"

  local meta_bin cli_version cli_bin
  meta_bin=$(jq -er '."app-cli-bin"' "$tool_root/bin/app-cli-meta.json") || fail "app-cli-meta.json has no app-cli-bin"
  cli_version=$(jq -er '."app-cli-version"' "$tool_root/bin/app-cli-meta.json") || fail "app-cli-meta.json has no app-cli-version"
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
  append_output app-cli-bin "$cli_bin"
  append_output app-cli-version "$cli_version"
  append_env EDGEZERO__ACTION__TOOL_ROOT "$tool_root"
}

main "$@"
