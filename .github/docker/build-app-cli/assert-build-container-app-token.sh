#!/usr/bin/env bash
set -euo pipefail

export LC_ALL=C
export BASH_ENV=
export ENV=

die() {
  printf '::error::%s\n' "$*" >&2
  exit 1
}

is_positive_u64() {
  local value=$1 maximum=18446744073709551615
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || return 1
  ((${#value} < ${#maximum})) && return 0
  # Equal-length canonical decimals are intentionally compared lexically.
  # shellcheck disable=SC2071
  ((${#value} == ${#maximum})) && [[ "$value" < "$maximum" || "$value" == "$maximum" ]]
}

[[ "$#" -eq 0 ]] || die "App token assertion accepts no arguments"
for name in EDGEZERO_APP_TOKEN EDGEZERO_INSTALLATION_ID EDGEZERO_EXPECTED_INSTALLATION_ID; do
  [[ -n "${!name:-}" ]] || die "required token context is absent: $name"
done
[[ "$EDGEZERO_APP_TOKEN" != *$'\n'* && "$EDGEZERO_APP_TOKEN" != *$'\r'* &&
  "$EDGEZERO_APP_TOKEN" != *'"'* && "$EDGEZERO_APP_TOKEN" != *\\* ]] ||
  die "App token cannot be encoded safely"
is_positive_u64 "$EDGEZERO_INSTALLATION_ID" || die "installation ID is not canonical"
is_positive_u64 "$EDGEZERO_EXPECTED_INSTALLATION_ID" ||
  die "expected installation ID is not canonical"
[[ "$EDGEZERO_INSTALLATION_ID" == "$EDGEZERO_EXPECTED_INSTALLATION_ID" ]] ||
  die "minted token came from an unexpected installation"

WORK_DIR=$(mktemp -d /tmp/edgezero-app-token.XXXXXX)
trap 'rm -rf -- "$WORK_DIR"' EXIT HUP INT TERM
CONFIG="$WORK_DIR/curl.config"
printf '%s\n' \
  'header = "Accept: application/vnd.github+json"' \
  'header = "X-GitHub-Api-Version: 2026-03-10"' \
  'header = "User-Agent: edgezero-build-container-gate/1"' \
  "header = \"Authorization: Bearer $EDGEZERO_APP_TOKEN\"" \
  >"$CONFIG"
REPLY="$WORK_DIR/reply"
env -i PATH="$PATH" LC_ALL=C curl \
  --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
  --request GET --config - \
  --write-out $'\n%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}' \
  https://api.github.com/repos/stackpop/edgezero \
  <"$CONFIG" >"$REPLY" || die "publisher token repository probe failed"

STATUS=$(tail -n 3 "$REPLY" | sed -n '1p')
SELECTED_VERSION=$(tail -n 3 "$REPLY" | sed -n '2p')
CONTENT_TYPE=$(tail -n 3 "$REPLY" | sed -n '3p')
[[ "$STATUS" == 200 ]] || die "publisher token probe returned HTTP $STATUS"
[[ "$SELECTED_VERSION" == 2026-03-10 ]] || die "GitHub selected an unexpected API version"
[[ "$CONTENT_TYPE" =~ ^[Aa][Pp][Pp][Ll][Ii][Cc][Aa][Tt][Ii][Oo][Nn]/[Jj][Ss][Oo][Nn]([[:space:]]*\;[[:space:]]*[Cc][Hh][Aa][Rr][Ss][Ee][Tt][[:space:]]*=[[:space:]]*[Uu][Tt][Ff]-8)?$ ]] ||
  die "GitHub returned an unsupported media type"
sed '$d' "$REPLY" | sed '$d' | sed '$d' >"$WORK_DIR/body"
jq -e '
  type == "object" and
  (.id | type == "number" and . >= 1 and . == floor) and
  .full_name == "stackpop/edgezero" and
  .private == false and .visibility == "public"
' "$WORK_DIR/body" >/dev/null 2>&1 || die "publisher token repository identity differs"
