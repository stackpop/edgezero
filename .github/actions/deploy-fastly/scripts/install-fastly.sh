#!/usr/bin/env bash
set -euo pipefail

# Installs the pinned Fastly CLI from an official release, verifying its SHA-256
# checksum, into an action-owned dir on PATH. This is the Fastly wrapper's
# provider-tool responsibility; the provider-neutral engine never installs it.
#
# Inputs (environment):
#   EDGEZERO_ACTION_ROOT  optional  repo root holding .tool-versions (defaults up)
#   VERSIONS_JSON         optional  pinned metadata (defaults alongside this dir)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

require_linux_x86_64() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) ;;
    *) fail "the Fastly wrapper supports only Linux x86-64 runners" ;;
  esac
}

main() {
  local action_dir
  action_dir=$(cd -- "$SCRIPT_DIR/.." && pwd)
  local action_root="${EDGEZERO_ACTION_ROOT:-$(cd -- "$action_dir/../../.." && pwd)}"
  local versions_json="${VERSIONS_JSON:-$action_dir/versions.json}"
  local tool_root="${EDGEZERO_TOOL_ROOT:-${RUNNER_TEMP:-/tmp}/edgezero-action-tools}"

  require_linux_x86_64
  require_cmd jq
  require_cmd curl
  mkdir -p "$tool_root/bin" "$tool_root/downloads"

  # The pinned version must agree with the repository .tool-versions policy.
  local version tool_version url sha256 archive
  version=$(json_get "$versions_json" fastly.version)
  tool_version=$(read_tool_version "$action_root/.tool-versions" fastly || true)
  [[ -n "$tool_version" ]] || fail "EdgeZero repository .tool-versions must contain a fastly entry"
  [[ "$version" == "$tool_version" ]] || fail "Fastly version mismatch: versions.json has $version but .tool-versions has $tool_version"

  url=$(json_get "$versions_json" fastly.linux_amd64.url)
  sha256=$(json_get "$versions_json" fastly.linux_amd64.sha256)
  archive="$tool_root/downloads/fastly-$version-linux-amd64.tar.gz"

  [[ -f "$archive" ]] || curl --fail --location --silent --show-error "$url" --output "$archive"

  local actual
  actual=$(sha256_file "$archive")
  [[ "$actual" == "$sha256" ]] || fail "Fastly CLI checksum mismatch for version $version"

  tar -xzf "$archive" -C "$tool_root/bin" fastly
  chmod +x "$tool_root/bin/fastly"
  printf '%s\n' "$tool_root/bin" >>"${GITHUB_PATH:-/dev/null}"
  export PATH="$tool_root/bin:$PATH"

  local provider_cli_version
  provider_cli_version=$(fastly version 2>/dev/null || fastly --version 2>/dev/null || true)
  provider_cli_version=${provider_cli_version%%$'\n'*}
  [[ -n "$provider_cli_version" ]] || fail "installed Fastly CLI did not report a version"
  notice "installed Fastly CLI: $provider_cli_version"
  append_output provider-cli-version "$provider_cli_version"
}

main "$@"
