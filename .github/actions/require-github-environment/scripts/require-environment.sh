#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

environment_name="${EDGEZERO__GITHUB__ENVIRONMENT:-}"
repository="${EDGEZERO__GITHUB__REPOSITORY:-}"
token="${EDGEZERO__GITHUB__TOKEN:-}"
api_url="${EDGEZERO__GITHUB__API_URL:-https://api.github.com}"

[[ -n "$environment_name" && "$environment_name" != *[$'\r\n\0']* ]] ||
  fail "environment-name must be non-empty and contain no control characters"
[[ "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] ||
  fail "repository must use owner/name form"
[[ -n "$token" && "$token" != *[$'\r\n']* ]] ||
  fail "github-token must be non-empty and contain no line breaks"
[[ "$api_url" =~ ^https://[^/]+(/[^[:space:]]*)?$ ]] ||
  fail "api-url must be an https URL"

for command in curl jq python3; do
  command -v "$command" >/dev/null 2>&1 || fail "required command '$command' was not found"
done

encoded_name=$(python3 - "$environment_name" <<'PY'
import sys
import urllib.parse

print(urllib.parse.quote(sys.argv[1], safe=""))
PY
)

response_file=$(mktemp "${RUNNER_TEMP:-/tmp}/edgezero-github-environment.XXXXXX")
trap 'rm -f -- "$response_file"' EXIT

status=0
http_code=$(printf 'Authorization: Bearer %s\n' "$token" | curl --silent --show-error \
  --output "$response_file" \
  --write-out '%{http_code}' \
  --header 'Accept: application/vnd.github+json' \
  --header @- \
  --header 'X-GitHub-Api-Version: 2022-11-28' \
  "$api_url/repos/$repository/environments/$encoded_name") || status=$?
[[ "$status" -eq 0 ]] || fail "GitHub Environment lookup failed before receiving a response"

case "$http_code" in
  200)
    jq -e --arg expected "$environment_name" \
      'type == "object" and .name == $expected' "$response_file" >/dev/null 2>&1 ||
      fail "GitHub Environment lookup returned a malformed or mismatched response"
    ;;
  404) fail "GitHub Environment '$environment_name' does not exist in '$repository'" ;;
  *) fail "GitHub Environment lookup failed with HTTP $http_code" ;;
esac

[[ -n "${GITHUB_OUTPUT:-}" ]] || fail "GITHUB_OUTPUT is required"
append_output environment-name "$environment_name"
