#!/usr/bin/env bash
set -euo pipefail

ACTION_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/edgezero-environment-test.XXXXXX")
trap 'rm -rf -- "$WORK_DIR"' EXIT

mkdir -p "$WORK_DIR/bin"
cat >"$WORK_DIR/bin/curl" <<'CURL'
#!/usr/bin/env bash
set -euo pipefail
output=""
url=""
while (($#)); do
  case "$1" in
    --output) output="$2"; shift 2 ;;
    --write-out | --header) shift 2 ;;
    --silent | --show-error) shift ;;
    *) url="$1"; shift ;;
  esac
done
cat >/dev/null
printf '%s' "$url" >"${FAKE_URL_OUT:?}"
case "${FAKE_RESPONSE:?}" in
  success) printf '{"name":"staging.app/example.com"}\n' >"$output"; printf 200 ;;
  missing) printf '{"message":"Not Found"}\n' >"$output"; printf 404 ;;
  forbidden) printf '{"message":"Forbidden"}\n' >"$output"; printf 403 ;;
  malformed) printf '[]\n' >"$output"; printf 200 ;;
  mismatch) printf '{"name":"production"}\n' >"$output"; printf 200 ;;
  network) exit 7 ;;
esac
CURL
chmod +x "$WORK_DIR/bin/curl"

run_case() {
  local response="$1"
  : >"$WORK_DIR/output"
  PATH="$WORK_DIR/bin:$PATH" \
    FAKE_RESPONSE="$response" \
    FAKE_URL_OUT="$WORK_DIR/url" \
    GITHUB_OUTPUT="$WORK_DIR/output" \
    RUNNER_TEMP="$WORK_DIR" \
    EDGEZERO__GITHUB__ENVIRONMENT='staging.app/example.com' \
    EDGEZERO__GITHUB__REPOSITORY='example/application' \
    EDGEZERO__GITHUB__TOKEN='test-token' \
    EDGEZERO__GITHUB__API_URL='https://api.github.test' \
    "$ACTION_DIR/scripts/require-environment.sh" >/dev/null 2>&1
}

run_case success
grep -qx 'environment-name=staging.app/example.com' "$WORK_DIR/output"
grep -Fqx 'https://api.github.test/repos/example/application/environments/staging.app%2Fexample.com' "$WORK_DIR/url"

for response in missing forbidden malformed mismatch network; do
  if run_case "$response"; then
    printf 'expected %s response to fail\n' "$response" >&2
    exit 1
  fi
done

if PATH="$WORK_DIR/bin:$PATH" \
  FAKE_RESPONSE=success \
  FAKE_URL_OUT="$WORK_DIR/url" \
  GITHUB_OUTPUT="$WORK_DIR/output" \
  RUNNER_TEMP="$WORK_DIR" \
  EDGEZERO__GITHUB__ENVIRONMENT=production \
  EDGEZERO__GITHUB__REPOSITORY=example/application \
  EDGEZERO__GITHUB__TOKEN=$'bad\nInjected: header' \
  EDGEZERO__GITHUB__API_URL=https://api.github.test \
  "$ACTION_DIR/scripts/require-environment.sh" >/dev/null 2>&1; then
  printf 'expected a line-breaking token to fail before curl\n' >&2
  exit 1
fi

printf 'GitHub Environment preflight tests passed\n'
