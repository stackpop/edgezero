#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

ADAPTER=${INPUT_ADAPTER:-}
BUILD_MODE=${INPUT_BUILD_MODE:-auto}
CACHE=${INPUT_CACHE:-false}
BUILD_ARGS=${INPUT_BUILD_ARGS:-[]}
DEPLOY_ARGS=${INPUT_DEPLOY_ARGS:-[]}
FASTLY_API_TOKEN_PRESENT=${INPUT_FASTLY_API_TOKEN_PRESENT:-false}
FASTLY_SERVICE_ID=${INPUT_FASTLY_SERVICE_ID:-}
RUNNER_OS_VALUE=${EDGEZERO_RUNNER_OS:-}
RUNNER_ARCH_VALUE=${EDGEZERO_RUNNER_ARCH:-}

if [[ -n "$RUNNER_OS_VALUE" || -n "$RUNNER_ARCH_VALUE" ]]; then
  [[ "$RUNNER_OS_VALUE" == "Linux" && "$RUNNER_ARCH_VALUE" == "X64" ]] || \
    fail "EdgeZero deploy v0 supports only Linux x86-64 runners; received ${RUNNER_OS_VALUE:-unknown}/${RUNNER_ARCH_VALUE:-unknown}"
fi

[[ -n "$ADAPTER" ]] || fail "input 'adapter' is required; v0 supports only 'fastly'"
case "$ADAPTER" in
  fastly) ;;
  axum) fail "adapter 'axum' has no EdgeZero remote deployment contract" ;;
  cloudflare|spin) fail "adapter '$ADAPTER' is planned for future work but is not implemented in v0; v0 supports only 'fastly'" ;;
  *) fail "unsupported adapter '$ADAPTER'; v0 supports only 'fastly'" ;;
esac

case "$BUILD_MODE" in
  auto|always|never) ;;
  *) fail "input 'build-mode' must be one of: auto, always, never" ;;
esac

case "$CACHE" in
  true|false) ;;
  *) fail "input 'cache' must be exactly 'true' or 'false'" ;;
esac

case "$FASTLY_API_TOKEN_PRESENT" in
  true|false) ;;
  *) fail "internal credential-presence value for fastly-api-token must be exactly 'true' or 'false'" ;;
esac
[[ "$FASTLY_API_TOKEN_PRESENT" == "true" ]] || fail "missing required input 'fastly-api-token'"
[[ -n "$FASTLY_SERVICE_ID" ]] || fail "missing required input 'fastly-service-id'"
python3 - "$FASTLY_SERVICE_ID" <<'PY'
import sys
value = sys.argv[1]
if any(ord(ch) < 32 or ord(ch) == 127 for ch in value):
    print("::error::input 'fastly-service-id' must not contain control characters", file=sys.stderr)
    sys.exit(1)
PY

parse_args() {
  local name="$1"
  local value="$2"
  local out="$3"
  python3 - "$name" "$value" "$out" <<'PY'
import json, sys
name, raw, out = sys.argv[1:4]
try:
    value = json.loads(raw)
except Exception as exc:
    print(f"::error::input '{name}' must be a JSON array of strings", file=sys.stderr)
    sys.exit(1)
if not isinstance(value, list):
    print(f"::error::input '{name}' must be a JSON array of strings", file=sys.stderr)
    sys.exit(1)
for item in value:
    if not isinstance(item, str):
        print(f"::error::every element of input '{name}' must be a string", file=sys.stderr)
        sys.exit(1)
    if '\x00' in item:
        print(f"::error::input '{name}' contains a NUL byte, which cannot be passed as an OS argument", file=sys.stderr)
        sys.exit(1)
with open(out, 'w', encoding='utf-8') as f:
    for item in value:
        f.write(item)
        f.write('\0')
PY
}

STATE_DIR=${EDGEZERO_ACTION_STATE_DIR:-${RUNNER_TEMP:-/tmp}/edgezero-action-state}
mkdir -p "$STATE_DIR"
BUILD_ARGS_FILE="$STATE_DIR/build-args.nul"
DEPLOY_ARGS_FILE="$STATE_DIR/deploy-args.nul"
parse_args "build-args" "$BUILD_ARGS" "$BUILD_ARGS_FILE"
parse_args "deploy-args" "$DEPLOY_ARGS" "$DEPLOY_ARGS_FILE"

reject_dangerous_deploy_args() {
  local file="$1"
  python3 - "$file" <<'PY'
import sys
with open(sys.argv[1], 'rb') as f:
    raw = f.read()
args = [a.decode('utf-8') for a in raw.split(b'\0') if a]
dangerous_exact = {
    '--service-id', '--service-name', '--token', '--api-token', '--profile',
    '--endpoint', '--api-endpoint', '--api-url', '--debug', '--debug-mode',
    '--verbose', '-v', '--interactive', '--non-interactive',
    '--accept-defaults', '--auto-yes'
}
dangerous_prefix = tuple(flag + '=' for flag in dangerous_exact if flag.startswith('--'))
for arg in args:
    if arg in dangerous_exact or arg.startswith(dangerous_prefix):
        print(f"::error::deploy-args contains rejected Fastly override flag '{arg.split('=', 1)[0]}'", file=sys.stderr)
        sys.exit(1)
PY
}
reject_dangerous_deploy_args "$DEPLOY_ARGS_FILE"

append_output adapter fastly
append_output build-args-file "$BUILD_ARGS_FILE"
append_output deploy-args-file "$DEPLOY_ARGS_FILE"
append_output requested-build-mode "$BUILD_MODE"
append_output cache "$CACHE"
