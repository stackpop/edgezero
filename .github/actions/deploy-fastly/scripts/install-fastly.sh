#!/usr/bin/env bash
set -euo pipefail

# Installs the pinned Fastly CLI from an official release, verifying its SHA-256
# checksum, into an action-owned dir on PATH. This is the Fastly wrapper's
# provider-tool responsibility; the provider-neutral engine never installs it.
#
# Reads (env):
#   EDGEZERO__ACTION__ROOT                optional  repo root holding .tool-versions (default: walk up)
#   EDGEZERO__FASTLY__VERSIONS_JSON       optional  pinned metadata (default: alongside this dir)
#   EDGEZERO__ACTION__TOOL_ROOT           optional  install dir (default: under RUNNER_TEMP)
# Writes (outputs):
#   provider-cli-version                  the installed Fastly CLI version
# Writes (PATH):
#   the tool root's bin/, so `fastly` is callable

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

require_linux_x86_64() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) ;;
    *) fail "the Fastly wrapper supports only Linux x86-64 runners" ;;
  esac
}

# The provider CLI gets its OWN directory, never the app CLI's `bin/`. The app
# names its own binary, and `fastly` is a legal app-CLI name -- sharing one
# directory would let this install silently overwrite the very CLI we are about
# to run. Both dirs live under the action-owned tool root, so cleanup still
# removes them together.
provider_bin_dir() { printf '%s\n' "$1/provider-bin"; }

main() {
  local action_dir
  action_dir=$(cd -- "$SCRIPT_DIR/.." && pwd)
  local action_root="${EDGEZERO__ACTION__ROOT:-$(cd -- "$action_dir/../../.." && pwd)}"
  local versions_json="${EDGEZERO__FASTLY__VERSIONS_JSON:-$action_dir/versions.json}"
  local tool_root="${EDGEZERO__ACTION__TOOL_ROOT:-${RUNNER_TEMP:-/tmp}/edgezero-action-tools}"

  require_linux_x86_64
  require_cmd jq
  require_cmd curl
  local provider_bin
  provider_bin=$(provider_bin_dir "$tool_root")
  mkdir -p "$provider_bin" "$tool_root/downloads"

  # The pinned version must agree with the repository .tool-versions policy.
  local version tool_version
  version=$(json_get "$versions_json" fastly.version)
  tool_version=$(read_tool_version "$action_root/.tool-versions" fastly || true)
  [[ -n "$tool_version" ]] || fail "EdgeZero repository .tool-versions must contain a fastly entry"
  [[ "$version" == "$tool_version" ]] || fail "Fastly version mismatch: versions.json has $version but .tool-versions has $tool_version"

  # The `fastly` binary is ALWAYS (re)extracted from a checksum-verified archive
  # — never adopted from a pre-existing binary. Trusting an already-present
  # binary by its version text would let an earlier step plant an arbitrary
  # executable in this predictable dir that then runs with FASTLY_API_TOKEN. The
  # ARCHIVE is the trust anchor: its SHA-256 must match `versions.json`, and the
  # binary is extracted from it every run.
  #
  # This stays idempotent WITHOUT trusting a binary: the archive is cached under
  # `downloads/`, so a second run in the same job re-verifies the cached archive
  # (cheap) and re-extracts rather than refetching.
  local url sha256 archive actual
  url=$(json_get "$versions_json" fastly.linux_amd64.url)
  sha256=$(json_get "$versions_json" fastly.linux_amd64.sha256)
  archive="$tool_root/downloads/fastly-$version-linux-amd64.tar.gz"

  [[ -f "$archive" ]] || curl --fail --location --silent --show-error "$url" --output "$archive"

  actual=$(sha256_file "$archive")
  [[ "$actual" == "$sha256" ]] || fail "Fastly CLI checksum mismatch for version $version"

  tar -xzf "$archive" -C "$provider_bin" fastly
  chmod +x "$provider_bin/fastly"

  printf '%s\n' "$provider_bin" >>"${GITHUB_PATH:-/dev/null}"
  export PATH="$provider_bin:$PATH"

  local provider_cli_version
  provider_cli_version=$(fastly version 2>/dev/null || fastly --version 2>/dev/null || true)
  provider_cli_version=${provider_cli_version%%$'\n'*}
  [[ -n "$provider_cli_version" ]] || fail "installed Fastly CLI did not report a version"
  notice "installed Fastly CLI: $provider_cli_version"
  append_output provider-cli-version "$provider_cli_version"
}

main "$@"
