#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

ACTION_DIR=$(cd -- "$SCRIPT_DIR/.." && pwd)
ACTION_ROOT=${EDGEZERO_ACTION_ROOT:-$(cd -- "$ACTION_DIR/../../.." && pwd)}
VERSIONS_JSON=${VERSIONS_JSON:-$ACTION_DIR/versions.json}
TOOL_ROOT=${EDGEZERO_TOOL_ROOT:-${RUNNER_TEMP:-/tmp}/edgezero-action-tools}
mkdir -p "$TOOL_ROOT/bin" "$TOOL_ROOT/downloads"

require_cmd python3
VERSION=$(json_get "$VERSIONS_JSON" fastly.version)
TOOL_VERSION=$(read_tool_version "$ACTION_ROOT/.tool-versions" fastly || true)
[[ -n "$TOOL_VERSION" ]] || fail "EdgeZero repository .tool-versions must contain a fastly entry"
[[ "$VERSION" == "$TOOL_VERSION" ]] || fail "Fastly version mismatch: versions.json has $VERSION but .tool-versions has $TOOL_VERSION"
URL=$(json_get "$VERSIONS_JSON" fastly.linux_amd64.url)
SHA256=$(json_get "$VERSIONS_JSON" fastly.linux_amd64.sha256)
ARCHIVE="$TOOL_ROOT/downloads/fastly-$VERSION-linux-amd64.tar.gz"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64|Linux-amd64) ;;
  *) fail "Fastly v0 action supports only Linux x86-64 runners" ;;
esac

if [[ ! -f "$ARCHIVE" ]]; then
  require_cmd curl
  curl --fail --location --silent --show-error "$URL" --output "$ARCHIVE"
fi

ACTUAL=$(python3 - "$ARCHIVE" <<'PY'
import hashlib, sys
h = hashlib.sha256()
with open(sys.argv[1], 'rb') as f:
    for chunk in iter(lambda: f.read(1024 * 1024), b''):
        h.update(chunk)
print(h.hexdigest())
PY
)
[[ "$ACTUAL" == "$SHA256" ]] || fail "Fastly CLI checksum mismatch for version $VERSION"

tar -xzf "$ARCHIVE" -C "$TOOL_ROOT/bin" fastly
chmod +x "$TOOL_ROOT/bin/fastly"
printf '%s\n' "$TOOL_ROOT/bin" >>"${GITHUB_PATH:-/dev/null}"
export PATH="$TOOL_ROOT/bin:$PATH"
PROVIDER_CLI_VERSION=$(fastly version 2>/dev/null || fastly --version 2>/dev/null || true)
[[ -n "$PROVIDER_CLI_VERSION" ]] || fail "installed Fastly CLI did not report a version"
printf '%s\n' "$PROVIDER_CLI_VERSION"
append_output provider-cli-version "$VERSION"
