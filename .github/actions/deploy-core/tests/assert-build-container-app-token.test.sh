#!/usr/bin/env bash
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ASSERT="$DIR/../../../docker/build-app-cli/assert-build-container-app-token.sh"
WORK=$(mktemp -d)
WORK=$(cd -- "$WORK" && pwd -P)
trap 'rm -rf "$WORK"' EXIT
FAKE_BIN="$WORK/bin"
mkdir -p "$FAKE_BIN"

cat >"$FAKE_BIN/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=$(cd -- "$(dirname -- "$0")" && pwd)
[[ "$LC_ALL" == C ]]
[[ -z "${GITHUB_TOKEN+x}${GH_TOKEN+x}${HTTPS_PROXY+x}${HTTP_PROXY+x}${ALL_PROXY+x}${HOME+x}${XDG_CONFIG_HOME+x}${CURL_HOME+x}" ]]
printf '%s\n' "$@" >"$fixture/args"
cat >"$fixture/config"
cat "$fixture/reply"
SH
chmod 0755 "$FAKE_BIN/curl"

pass=0
fail=0

write_reply() {
  local full_name=${1:-stackpop/edgezero} status=${2:-200}
  local version=${3:-2026-03-10} media=${4:-application/json}
  jq -cn --arg full_name "$full_name" \
    '{id:123456789,full_name:$full_name,private:false,visibility:"public"}' >"$FAKE_BIN/body"
  {
    cat "$FAKE_BIN/body"
    printf '\n%s\n%s\n%s' "$status" "$version" "$media"
  } >"$FAKE_BIN/reply"
}

run_assert() {
  PATH="$FAKE_BIN:$PATH" \
    EDGEZERO_APP_TOKEN="${TOKEN:-fixture-token}" \
    EDGEZERO_INSTALLATION_ID="${INSTALLATION_ID:-12345}" \
    EDGEZERO_EXPECTED_INSTALLATION_ID="${EXPECTED_INSTALLATION_ID:-12345}" \
    bash "$ASSERT"
}

assert_pass() {
  local description=$1
  if run_assert >"$WORK/stdout" 2>"$WORK/stderr" &&
    [[ ! -s "$WORK/stdout" && ! -s "$WORK/stderr" ]]; then
    printf '  \033[32mok\033[0m   %s\n' "$description"
    pass=$((pass + 1))
  else
    cat "$WORK/stdout" "$WORK/stderr" >&2
    printf '  \033[31mFAIL\033[0m %s\n' "$description" >&2
    fail=$((fail + 1))
  fi
}

assert_fail() {
  local description=$1
  if run_assert >"$WORK/stdout" 2>"$WORK/stderr"; then
    printf '  \033[31mFAIL\033[0m %s\n' "$description" >&2
    fail=$((fail + 1))
  elif [[ -s "$WORK/stdout" ]]; then
    cat "$WORK/stdout" >&2
    printf '  \033[31mFAIL\033[0m %s emitted stdout\n' "$description" >&2
    fail=$((fail + 1))
  else
    printf '  \033[32mok\033[0m   %s\n' "$description"
    pass=$((pass + 1))
  fi
}

echo '== protected publisher App token probe =='
write_reply
assert_pass 'exact installation token can read only the expected repository identity'
if grep -qF 'Authorization: Bearer fixture-token' "$FAKE_BIN/config" &&
  ! grep -qF 'fixture-token' "$FAKE_BIN/args"; then
  printf '  \033[32mok\033[0m   token is passed through config stdin and never argv\n'
  pass=$((pass + 1))
else
  printf '  \033[31mFAIL\033[0m token is passed through config stdin and never argv\n' >&2
  fail=$((fail + 1))
fi
if awk 'previous == "--connect-timeout" && $0 == "10" { found = 1 } { previous = $0 } END { exit !found }' \
  "$FAKE_BIN/args"; then
  printf '  \033[32mok\033[0m   API probe fixes the connection timeout at 10 seconds\n'
  pass=$((pass + 1))
else
  printf '  \033[31mFAIL\033[0m API probe fixes the connection timeout at 10 seconds\n' >&2
  fail=$((fail + 1))
fi

INSTALLATION_ID=12346
assert_fail 'action installation output must equal protected environment value'
unset INSTALLATION_ID
EXPECTED_INSTALLATION_ID=012345
assert_fail 'installation IDs must be canonical positive integers'
unset EXPECTED_INSTALLATION_ID
TOKEN=$'bad\ntoken'
assert_fail 'line-breaking token is rejected'
unset TOKEN

write_reply other/repository
assert_fail 'repository substitution is rejected'
write_reply stackpop/edgezero 302
assert_fail 'redirect response is rejected'
write_reply stackpop/edgezero 200 2022-11-28
assert_fail 'wrong selected API version is rejected'
write_reply stackpop/edgezero 200 2026-03-10 text/json
assert_fail 'wrong response media type is rejected'

if ((fail)); then
  printf '\n%d passed, %d failed\n' "$pass" "$fail" >&2
  exit 1
fi
printf '\n%d passed, %d failed\n' "$pass" "$fail"
