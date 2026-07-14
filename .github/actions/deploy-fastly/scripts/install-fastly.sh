#!/usr/bin/env bash
set -euo pipefail

# Installs the pinned Fastly CLI from an official release, verifying its SHA-256
# checksum, into an action-owned dir on PATH. This is the Fastly wrapper's
# provider-tool responsibility; the provider-neutral engine never installs it.
#
# Inputs (environment):
#   EDGEZERO__ACTION__ROOT               optional repo root holding .tool-versions (defaults up)
#   EDGEZERO__FASTLY__VERSIONS_JSON      optional pinned metadata (defaults alongside this dir)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

require_linux_x86_64() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) ;;
    *) fail "the Fastly wrapper supports only Linux x86-64 runners" ;;
  esac
}

# Whether the action-owned tool root already holds an executable `fastly`
# reporting the pinned version.
tool_root_has_pinned_fastly() {
  local tool_root="$1" version="$2"
  local bin="$tool_root/bin/fastly"

  [[ -x "$bin" ]] || return 1
  local reported
  reported=$("$bin" version 2>/dev/null | head -n 1) || return 1
  [[ "$reported" == *"$version"* ]] || return 1

  notice "reusing the already-installed Fastly CLI: $reported"
  return 0
}

main() {
  local action_dir
  action_dir=$(cd -- "$SCRIPT_DIR/.." && pwd)
  local action_root="${EDGEZERO__ACTION__ROOT:-$(cd -- "$action_dir/../../.." && pwd)}"
  local versions_json="${EDGEZERO__FASTLY__VERSIONS_JSON:-$action_dir/versions.json}"
  local tool_root="${EDGEZERO__ACTION__TOOL_ROOT:-${RUNNER_TEMP:-/tmp}/edgezero-action-tools}"

  require_linux_x86_64
  require_cmd jq
  require_cmd curl
  mkdir -p "$tool_root/bin" "$tool_root/downloads"

  # The pinned version must agree with the repository .tool-versions policy.
  local version tool_version
  version=$(json_get "$versions_json" fastly.version)
  tool_version=$(read_tool_version "$action_root/.tool-versions" fastly || true)
  [[ -n "$tool_version" ]] || fail "EdgeZero repository .tool-versions must contain a fastly entry"
  [[ "$version" == "$tool_version" ]] || fail "Fastly version mismatch: versions.json has $version but .tool-versions has $tool_version"

  # Idempotent: if the action-owned tool root already holds a `fastly` reporting
  # the pinned version, adopt it instead of downloading again. Running the
  # wrapper twice in one job should not refetch a binary we already verified.
  # The scope is deliberately narrow — only this dir, which the action creates
  # under RUNNER_TEMP, populates, executes from, and deletes on cleanup. A
  # `fastly` found merely on PATH is NOT trusted and is always superseded.
  if ! tool_root_has_pinned_fastly "$tool_root" "$version"; then
    local url sha256 archive
    url=$(json_get "$versions_json" fastly.linux_amd64.url)
    sha256=$(json_get "$versions_json" fastly.linux_amd64.sha256)
    archive="$tool_root/downloads/fastly-$version-linux-amd64.tar.gz"

    [[ -f "$archive" ]] || curl --fail --location --silent --show-error "$url" --output "$archive"

    local actual
    actual=$(sha256_file "$archive")
    [[ "$actual" == "$sha256" ]] || fail "Fastly CLI checksum mismatch for version $version"

    tar -xzf "$archive" -C "$tool_root/bin" fastly
    chmod +x "$tool_root/bin/fastly"
  fi

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
