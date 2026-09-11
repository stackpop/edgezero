#!/usr/bin/env bash
# shellcheck disable=SC2016,SC2119,SC2120
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REAL_GATE="$DIR/../../../docker/build-app-cli/release-approval-gate.sh"
WORK=$(mktemp -d)
WORK=$(cd -- "$WORK" && pwd -P)
trap 'rm -rf -- "$WORK"' EXIT

RUN_ID=9007199254740993
RUN_ATTEMPT=2
SOURCE=$(printf '2%.0s' {1..40})
TAG=build-container-v7
DIGEST="sha256:$(printf '3%.0s' {1..64})"
CHALLENGE=$(printf '4%.0s' {1..64})
SCREENSHOT="sha256:$(printf '5%.0s' {1..64})"
REVIEWED_AT=2026-09-10T11:55:00Z
NOW=2026-09-10T12:00:00Z
TOKEN_VALUE='fixture-token-value'

pass=0
fail=0
case_number=0

ok() {
  printf '  \033[32mok\033[0m   %s\n' "$1"
  pass=$((pass + 1))
}

no() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1" >&2
  fail=$((fail + 1))
}

GATE_ROOT="$WORK/gate"
FAKE_BIN="$WORK/fake-bin"
OUTPUT_ROOT="$WORK/private-output"
mkdir -p "$GATE_ROOT/.github/docker/build-app-cli" "$FAKE_BIN" "$OUTPUT_ROOT"
chmod 0700 "$OUTPUT_ROOT"
cp "$REAL_GATE" "$GATE_ROOT/.github/docker/build-app-cli/release-approval-gate.sh"
chmod 0755 "$GATE_ROOT/.github/docker/build-app-cli/release-approval-gate.sh"
git -C "$GATE_ROOT" init -q -b main
git -C "$GATE_ROOT" config user.name fixture
git -C "$GATE_ROOT" config user.email fixture@example.invalid
git -C "$GATE_ROOT" add .
git -C "$GATE_ROOT" commit -q -m gate
G=$(git -C "$GATE_ROOT" rev-parse HEAD)
git -C "$GATE_ROOT" checkout -q --detach "$G"
GATE="$GATE_ROOT/.github/docker/build-app-cli/release-approval-gate.sh"

cat >"$FAKE_BIN/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=$(cd -- "$(dirname -- "$0")" && pwd)
[[ "${LC_ALL:-}" == C ]]
[[ -z "${GITHUB_TOKEN+x}${GH_TOKEN+x}${HOME+x}${CURL_HOME+x}${XDG_CONFIG_HOME+x}" ]]
[[ -z "${HTTP_PROXY+x}${HTTPS_PROXY+x}${ALL_PROXY+x}${NO_PROXY+x}${AMBIENT_SECRET+x}" ]]
count=0
[[ ! -f "$fixture/curl-count" ]] || count=$(<"$fixture/curl-count")
count=$((count + 1))
printf '%s' "$count" >"$fixture/curl-count"
printf '%s\n' "$@" >"$fixture/args-$count"
cat >"$fixture/config-$count"
output=
url=
while (($#)); do
  case "$1" in
    --output)
      output=$2
      shift 2
      ;;
    *)
      url=$1
      shift
      ;;
  esac
done
[[ -n "$output" ]]
case "$url" in
  */approvals) endpoint=approvals ;;
  *) endpoint=run ;;
esac
if [[ -f "$fixture/$endpoint.transport-failure" ]]; then
  printf '%s\n' 'curl: transport response detail must not leak' >&2
  exit 28
fi
if [[ -f "$fixture/$endpoint.block" ]]; then
  printf '%s' "$$" >"$fixture/curl-child-pid"
  : >"$fixture/curl-blocked"
  while [[ ! -f "$fixture/release-curl" ]]; do :; done
fi
cat "$fixture/$endpoint.body" >"$output"
if [[ -f "$fixture/$endpoint.raw-metadata" ]]; then
  cat "$fixture/$endpoint.raw-metadata"
else
  printf '%s\n%s\n%s' \
    "$(<"$fixture/$endpoint.status")" \
    "$(<"$fixture/$endpoint.version")" \
    "$(<"$fixture/$endpoint.media")"
fi
SH
chmod 0755 "$FAKE_BIN/curl"

cat >"$FAKE_BIN/date" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=$(cd -- "$(dirname -- "$0")" && pwd)
[[ "${LC_ALL:-}" == C ]]
[[ -z "${GITHUB_TOKEN+x}${GH_TOKEN+x}${HOME+x}${AMBIENT_SECRET+x}" ]]
[[ "$#" -eq 2 && "$1" == -u && "$2" == +%s ]]
cat "$fixture/now-epoch"
SH
chmod 0755 "$FAKE_BIN/date"
jq -nr --arg value "$NOW" '$value | fromdateiso8601' >"$FAKE_BIN/now-epoch"

REAL_GIT=$(command -v git)
{
  printf '#!/usr/bin/env bash\nset -euo pipefail\n'
  printf 'real_git=%q\n' "$REAL_GIT"
  printf '%s\n' \
    'fixture=$(cd -- "$(dirname -- "$0")" && pwd)' \
    'if [[ -f "$fixture/fail-git-status" && " $* " == *" status "* ]]; then exit 1; fi' \
    'exec "$real_git" "$@"'
} >"$FAKE_BIN/git"
chmod 0755 "$FAKE_BIN/git"

REAL_MKTEMP=$(command -v mktemp)
{
  printf '#!/usr/bin/env bash\nset -euo pipefail\n'
  printf 'real_mktemp=%q\n' "$REAL_MKTEMP"
  printf '%s\n' \
    'fixture=$(cd -- "$(dirname -- "$0")" && pwd)' \
    'count=0' \
    '[[ ! -f "$fixture/mktemp-count" ]] || count=$(<"$fixture/mktemp-count")' \
    'count=$((count + 1))' \
    'printf "%s" "$count" >"$fixture/mktemp-count"' \
    'if [[ -f "$fixture/fail-metadata-mktemp" && "$count" -eq 2 ]]; then exit 1; fi' \
    'path=$("$real_mktemp" "$@")' \
    'printf "%s\n" "$path" >>"$fixture/mktemp-created"' \
    'printf "%s\n" "$path"'
} >"$FAKE_BIN/mktemp"
chmod 0755 "$FAKE_BIN/mktemp"

expected_comment() {
  local reviewed_at=${1:-$REVIEWED_AT} screenshot=${2:-$SCREENSHOT}
  printf '%s' "edgezero-release-evidence-v1 {\"challenge\":\"$CHALLENGE\",\"image-digest\":\"$DIGEST\",\"png-sha256\":\"$screenshot\",\"release-tag\":\"$TAG\",\"reviewed-at\":\"$reviewed_at\",\"run-attempt\":\"$RUN_ATTEMPT\",\"run-id\":\"$RUN_ID\",\"source-revision\":\"$SOURCE\"}"
}

review_json() {
  local state=$1 environment=$2 comment=$3 login=${4:-release-reviewer}
  jq -cn \
    --arg state "$state" \
    --arg environment "$environment" \
    --arg comment "$comment" \
    --arg login "$login" \
    '{environments:[{name:$environment}],state:$state,user:{login:$login},comment:$comment}'
}

write_approvals() {
  local separator='' item
  printf '[' >"$FAKE_BIN/approvals.body"
  for item in "$@"; do
    printf '%s%s' "$separator" "$item" >>"$FAKE_BIN/approvals.body"
    separator=,
  done
  printf ']' >>"$FAKE_BIN/approvals.body"
}

write_current_review() {
  local state=${1:-approved} environment=${2:-build-container-release}
  local comment=${3:-$(expected_comment)} login=${4:-release-reviewer}
  write_approvals "$(review_json "$state" "$environment" "$comment" "$login")"
}

write_run_body() {
  local body=${1:-"{\"id\":$RUN_ID,\"run_attempt\":$RUN_ATTEMPT,\"event\":\"workflow_dispatch\",\"status\":\"completed\",\"conclusion\":\"failure\",\"head_sha\":\"wrong\",\"path\":\"wrong\"}"}
  printf '%s' "$body" >"$FAKE_BIN/run.body"
}

set_metadata() {
  local endpoint=$1 status=${2:-200} version=${3:-2026-03-10} media=${4:-application/json}
  printf '%s' "$status" >"$FAKE_BIN/$endpoint.status"
  printf '%s' "$version" >"$FAKE_BIN/$endpoint.version"
  printf '%s' "$media" >"$FAKE_BIN/$endpoint.media"
}

reset_api() {
  rm -f -- "$FAKE_BIN"/args-* "$FAKE_BIN"/config-* "$FAKE_BIN/curl-count" \
    "$FAKE_BIN/fail-git-status" "$FAKE_BIN/fail-metadata-mktemp" \
    "$FAKE_BIN/mktemp-count" "$FAKE_BIN/mktemp-created" \
    "$FAKE_BIN/run.raw-metadata" "$FAKE_BIN/approvals.raw-metadata" \
    "$FAKE_BIN/run.transport-failure" "$FAKE_BIN/approvals.transport-failure" \
    "$FAKE_BIN/run.block" "$FAKE_BIN/approvals.block" "$FAKE_BIN/curl-child-pid" \
    "$FAKE_BIN/curl-blocked" "$FAKE_BIN/release-curl"
  write_run_body
  write_current_review
  set_metadata run
  set_metadata approvals
}

new_case() {
  case_number=$((case_number + 1))
  CASE_ROOT="$WORK/case-$case_number"
  mkdir "$CASE_ROOT"
  APPROVAL_OUT="$OUTPUT_ROOT/approval-$case_number.json"
  CLI_GATE_SHA=$G
  CLI_RUN_ID=$RUN_ID
  CLI_RUN_ATTEMPT=$RUN_ATTEMPT
  CLI_BUILD_ATTEMPT=$RUN_ATTEMPT
  CLI_SOURCE=$SOURCE
  CLI_TAG=$TAG
  CLI_DIGEST=$DIGEST
  CLI_CHALLENGE=$CHALLENGE
  TOKEN_VALUE='fixture-token-value'
  reset_api
}

run_gate() {
  PATH="$FAKE_BIN:$PATH" \
    GITHUB_TOKEN="$TOKEN_VALUE" \
    AMBIENT_SECRET=must-not-reach-tools \
    bash "$GATE" \
      --gate-root "$GATE_ROOT" \
      --gate-sha "$CLI_GATE_SHA" \
      --run-id "$CLI_RUN_ID" \
      --run-attempt "$CLI_RUN_ATTEMPT" \
      --build-attempt "$CLI_BUILD_ATTEMPT" \
      --source-revision "$CLI_SOURCE" \
      --release-tag "$CLI_TAG" \
      --image-digest "$CLI_DIGEST" \
      --approval-challenge "$CLI_CHALLENGE" \
      --approval-out "$APPROVAL_OUT" \
      "$@"
}

start_gate() {
  local bash_env=$1
  PATH="$FAKE_BIN:$PATH" \
    BASH_ENV="$bash_env" \
    SIGNAL_FIXTURE="$CASE_ROOT" \
    GITHUB_TOKEN="$TOKEN_VALUE" \
    AMBIENT_SECRET=must-not-reach-tools \
    /bin/bash "$GATE" \
      --gate-root "$GATE_ROOT" \
      --gate-sha "$CLI_GATE_SHA" \
      --run-id "$CLI_RUN_ID" \
      --run-attempt "$CLI_RUN_ATTEMPT" \
      --build-attempt "$CLI_BUILD_ATTEMPT" \
      --source-revision "$CLI_SOURCE" \
      --release-tag "$CLI_TAG" \
      --image-digest "$CLI_DIGEST" \
      --approval-challenge "$CLI_CHALLENGE" \
      --approval-out "$APPROVAL_OUT" \
      >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" &
  SIGNAL_GATE_PID=$!
}

wait_for_marker() {
  local marker=$1 pid=$2 deadline=$((SECONDS + 3))
  while [[ ! -e "$marker" && $SECONDS -lt $deadline ]]; do
    kill -0 "$pid" 2>/dev/null || return 1
  done
  [[ -e "$marker" ]]
}

wait_for_signaled_gate() {
  local gate_pid=$1 child_pid_file=${2:-} watchdog status=0
  (
    deadline=$((SECONDS + 3))
    while ((SECONDS < deadline)); do :; done
    : >"$CASE_ROOT/watchdog-fired"
    kill -KILL "$gate_pid" 2>/dev/null || true
    if [[ -n "$child_pid_file" && -s "$child_pid_file" ]]; then
      kill -KILL "$(<"$child_pid_file")" 2>/dev/null || true
    fi
  ) &
  watchdog=$!
  wait "$gate_pid" || status=$?
  kill -TERM "$watchdog" 2>/dev/null || true
  wait "$watchdog" 2>/dev/null || true
  SIGNAL_GATE_STATUS=$status
}

assert_signal_cleanup() {
  local expected_status=$1 description=$2 residue
  residue=$(find "$OUTPUT_ROOT" -maxdepth 1 -name '.edgezero-*' -print -quit)
  if [[ "$SIGNAL_GATE_STATUS" -ne "$expected_status" ]]; then
    cat "$CASE_ROOT/stdout" "$CASE_ROOT/stderr" >&2
    no "$description (status $SIGNAL_GATE_STATUS, expected $expected_status)"
  elif [[ -e "$CASE_ROOT/watchdog-fired" ]]; then
    no "$description required the test watchdog"
  elif [[ -s "$CASE_ROOT/stdout" || -e "$APPROVAL_OUT" || -L "$APPROVAL_OUT" || -n "$residue" ]]; then
    cat "$CASE_ROOT/stdout" "$CASE_ROOT/stderr" >&2
    no "$description left output or temporary-file residue"
  else
    ok "$description"
  fi
}

run_gate_missing_challenge() {
  PATH="$FAKE_BIN:$PATH" GITHUB_TOKEN="$TOKEN_VALUE" bash "$GATE" \
    --gate-root "$GATE_ROOT" --gate-sha "$G" --run-id "$CLI_RUN_ID" \
    --run-attempt "$CLI_RUN_ATTEMPT" --build-attempt "$CLI_BUILD_ATTEMPT" \
    --source-revision "$CLI_SOURCE" --release-tag "$CLI_TAG" \
    --image-digest "$CLI_DIGEST" --approval-out "$APPROVAL_OUT"
}

run_bad_flag_without_tools() {
  mkdir "$CASE_ROOT/empty-path"
  PATH="$CASE_ROOT/empty-path" GITHUB_TOKEN=$'bad\ntoken' /bin/bash "$GATE" \
    --gate-root "$GATE_ROOT" --gate-sha "$G" --run-id "$CLI_RUN_ID" \
    --run-attempt "$CLI_RUN_ATTEMPT" --build-attempt "$CLI_BUILD_ATTEMPT" \
    --source-revision "$CLI_SOURCE" --release-tag "$CLI_TAG" \
    --image-digest "$CLI_DIGEST" --approval-challenge "$CLI_CHALLENGE" \
    --approval-out "$APPROVAL_OUT" --unknown value
}

run_gate_without_tools() {
  mkdir "$CASE_ROOT/empty-path"
  PATH="$CASE_ROOT/empty-path" GITHUB_TOKEN=$'bad\ntoken' /bin/bash "$GATE" \
    --gate-root "$GATE_ROOT" --gate-sha "$CLI_GATE_SHA" --run-id "$CLI_RUN_ID" \
    --run-attempt "$CLI_RUN_ATTEMPT" --build-attempt "$CLI_BUILD_ATTEMPT" \
    --source-revision "$CLI_SOURCE" --release-tag "$CLI_TAG" \
    --image-digest "$CLI_DIGEST" --approval-challenge "$CLI_CHALLENGE" \
    --approval-out "$APPROVAL_OUT"
}

curl_calls() {
  if [[ -f "$FAKE_BIN/curl-count" ]]; then cat "$FAKE_BIN/curl-count"; else printf '0'; fi
}

assert_result() {
  local expected_status=$1 description=$2 status=0
  shift 2
  "$@" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$?
  if [[ "$status" -ne "$expected_status" ]]; then
    cat "$CASE_ROOT/stdout" "$CASE_ROOT/stderr" >&2
    no "$description (status $status, expected $expected_status)"
  elif [[ -s "$CASE_ROOT/stdout" ]]; then
    cat "$CASE_ROOT/stdout" >&2
    no "$description emitted stdout"
  elif [[ "$expected_status" -eq 0 && (! -f "$APPROVAL_OUT" || -L "$APPROVAL_OUT" || -s "$CASE_ROOT/stderr") ]]; then
    cat "$CASE_ROOT/stderr" >&2
    no "$description did not produce one silent regular output"
  elif [[ "$expected_status" -ne 0 && (-e "$APPROVAL_OUT" || -L "$APPROVAL_OUT") ]]; then
    no "$description published output on failure"
  elif [[ "$expected_status" -ne 0 && (! -s "$CASE_ROOT/stderr" || $(<"$CASE_ROOT/stderr") == *"$TOKEN_VALUE"*) ]]; then
    cat "$CASE_ROOT/stderr" >&2
    no "$description did not emit sanitized stderr"
  else
    ok "$description"
  fi
}

assert_no_curl() {
  local description=$1
  if [[ $(curl_calls) == 0 ]]; then ok "$description"; else no "$description"; fi
}

assert_curl_contract() {
  local good=true call args config expected_config url body index
  local -a actual expected
  expected_config="$CASE_ROOT/expected-config"
  printf '%s\n' \
    'header = "Accept: application/vnd.github+json"' \
    'header = "X-GitHub-Api-Version: 2026-03-10"' \
    'header = "User-Agent: edgezero-build-container-gate/1"' \
    "header = \"Authorization: Bearer $TOKEN_VALUE\"" \
    >"$expected_config"
  [[ $(curl_calls) == 2 ]] || good=false
  for call in 1 2; do
    args="$FAKE_BIN/args-$call"
    config="$FAKE_BIN/config-$call"
    [[ -f "$args" && -f "$config" ]] || {
      good=false
      continue
    }
    cmp -s "$expected_config" "$config" || good=false
    actual=()
    while IFS= read -r argument || [[ -n "$argument" ]]; do
      actual+=("$argument")
    done <"$args"
    body=${actual[14]:-}
    if [[ "$call" -eq 1 ]]; then
      url="https://api.github.com/repos/stackpop/edgezero/actions/runs/$RUN_ID"
    else
      url="https://api.github.com/repos/stackpop/edgezero/actions/runs/$RUN_ID/approvals"
    fi
    expected=(
      --disable
      --silent
      --show-error
      --connect-timeout 10
      --max-time 30
      --max-redirs 0
      --request GET
      --config -
      --output "$body"
      --write-out '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}'
      "$url"
    )
    [[ "$body" == "$OUTPUT_ROOT"/.edgezero-github-body.?????? ]] || good=false
    [[ "${#actual[@]}" -eq "${#expected[@]}" ]] || good=false
    for index in "${!expected[@]}"; do
      [[ "${actual[index]:-}" == "${expected[index]}" ]] || good=false
    done
  done
  if [[ "$good" == true ]]; then
    ok 'both API calls use the exact transport, headers, GET routes, and stdin credentials'
  else
    no 'both API calls use the exact transport, headers, GET routes, and stdin credentials'
  fi
}

echo '== release approval gate =='

new_case
assert_result 0 'a current approved review writes evidence silently' run_gate
expected_output="{\"approval-challenge\":\"$CHALLENGE\",\"approver-login\":\"release-reviewer\",\"image-digest\":\"$DIGEST\",\"release-tag\":\"$TAG\",\"reviewed-at\":\"$REVIEWED_AT\",\"run-attempt\":\"$RUN_ATTEMPT\",\"run-id\":\"$RUN_ID\",\"schema-version\":1,\"screenshot-sha256\":\"$SCREENSHOT\",\"source-revision\":\"$SOURCE\"}"
printf '%s' "$expected_output" >"$CASE_ROOT/expected-output"
if cmp -s "$CASE_ROOT/expected-output" "$APPROVAL_OUT"; then
  ok 'approval evidence is exact JCS with no trailing LF'
else
  no 'approval evidence is exact JCS with no trailing LF'
fi
assert_curl_contract
if ! grep -Fq -- "$TOKEN_VALUE" "$CASE_ROOT/stdout" &&
  ! grep -Fq -- "$TOKEN_VALUE" "$CASE_ROOT/stderr"; then
  ok 'the token is absent from gate stdout and stderr'
else
  no 'the token is absent from gate stdout and stderr'
fi

for mode in unknown duplicate empty missing; do
  new_case
  case "$mode" in
    unknown) assert_result 2 'an unknown flag is rejected as usage' run_gate --unknown value ;;
    duplicate) assert_result 2 'a duplicate flag is rejected as usage' run_gate --run-id 7 ;;
    empty)
      CLI_RUN_ID=
      assert_result 2 'an empty flag value is rejected as usage' run_gate
      ;;
    missing) assert_result 2 'a missing required flag is rejected as usage' run_gate_missing_challenge ;;
  esac
  assert_no_curl "$mode flag failure occurs before curl"
done

new_case
assert_result 2 'bad flags take precedence when tools and credentials are invalid' \
  run_bad_flag_without_tools
assert_no_curl 'bad flags with unavailable tools do not call curl'

new_case
CLI_BUILD_ATTEMPT=1
assert_result 1 'build attempt must equal the current run attempt' run_gate
assert_no_curl 'build/run attempt mismatch occurs before curl'

while read -r field value; do
  new_case
  case "$field" in
    run-id) CLI_RUN_ID=$value ;;
    run-attempt) CLI_RUN_ATTEMPT=$value ;;
    build-attempt) CLI_BUILD_ATTEMPT=$value ;;
  esac
  assert_result 1 "$field value $value is rejected before curl" run_gate
  assert_no_curl "$field value $value does not call curl"
done <<'EOF'
run-id 0
run-id 01
run-id 18446744073709551616
run-attempt 0
run-attempt 02
run-attempt 4294967296
build-attempt 0
build-attempt 02
build-attempt 4294967296
EOF

new_case
assert_result 2 'an unavailable required tool exits 2 before credential access' \
  run_gate_without_tools
assert_no_curl 'an unavailable required tool does not call curl'

new_case
malformed='edgezero-release-evidence-v1 {"challenge":'
write_current_review approved build-container-release "$malformed"
assert_result 1 'a malformed protocol comment is rejected' run_gate

new_case
reordered=$(expected_comment)
reordered=${reordered/\{\"challenge\":\"$CHALLENGE\",\"image-digest\":\"$DIGEST\"/\{\"image-digest\":\"$DIGEST\",\"challenge\":\"$CHALLENGE\"}
write_current_review approved build-container-release "$reordered"
assert_result 1 'a reordered current protocol object is rejected' run_gate

for duplicate_case in current-earlier current-future challenge image-digest; do
  new_case
  duplicate=$(expected_comment)
  case "$duplicate_case" in
    current-earlier)
      replacement='"run-attempt":"2","run-attempt":"1"'
      duplicate_description='run-attempt keys straddling current and earlier attempts'
      ;;
    current-future)
      replacement='"run-attempt":"2","run-attempt":"3"'
      duplicate_description='run-attempt keys straddling current and future attempts'
      ;;
    challenge)
      replacement="\"challenge\":\"$CHALLENGE\",\"challenge\":\"$(printf '6%.0s' {1..64})\""
      duplicate_description='challenge keys'
      ;;
    image-digest)
      replacement="\"image-digest\":\"$DIGEST\",\"image-digest\":\"sha256:$(printf '6%.0s' {1..64})\""
      duplicate_description='image-digest keys'
      ;;
  esac
  case "$duplicate_case" in
    current-*) duplicate=${duplicate/"\"run-attempt\":\"$RUN_ATTEMPT\""/"$replacement"} ;;
    challenge) duplicate=${duplicate/"\"challenge\":\"$CHALLENGE\""/"$replacement"} ;;
    image-digest) duplicate=${duplicate/"\"image-digest\":\"$DIGEST\""/"$replacement"} ;;
  esac
  write_approvals \
    "$(review_json approved build-container-release "$duplicate")" \
    "$(review_json approved build-container-release "$(expected_comment)")"
  assert_result 1 "duplicate $duplicate_description are rejected" run_gate
  if grep -Fq 'duplicate or malformed protocol JSON keys' "$CASE_ROOT/stderr"; then
    ok "duplicate $duplicate_case keys fail in the raw protocol parser"
  else
    cat "$CASE_ROOT/stderr" >&2
    no "duplicate $duplicate_case keys fail in the raw protocol parser"
  fi
done

for shape in extra missing-source-revision missing-challenge missing-image-digest; do
  new_case
  comment=$(expected_comment)
  case "$shape" in
    extra) comment="${comment%\}},\"extra\":\"value\"}" ;;
    missing-source-revision) comment=${comment/,\"source-revision\":\"$SOURCE\"/} ;;
    missing-challenge) comment=${comment/\"challenge\":\"$CHALLENGE\",/} ;;
    missing-image-digest) comment=${comment/,\"image-digest\":\"$DIGEST\"/} ;;
  esac
  write_current_review approved build-container-release "$comment"
  assert_result 1 "a current protocol record with $shape is rejected" run_gate
done

new_case
write_current_review approved build-container-release \
  "$(expected_comment "$REVIEWED_AT" 'sha256:not-a-digest')"
assert_result 1 'an invalid PNG digest is rejected' run_gate

new_case
current=$(review_json approved build-container-release "$(expected_comment)")
write_approvals "$current" "$current"
assert_result 1 'duplicate current protocol records are rejected' run_gate

for state in rejected bypassed; do
  new_case
  write_current_review "$state"
  assert_result 1 "a $state current review is rejected" run_gate
done

new_case
write_current_review approved production
assert_result 1 'a review for the wrong environment is rejected' run_gate

for reviewer_case in malformed-login malformed-user; do
  new_case
  case "$reviewer_case" in
    malformed-login)
      write_current_review approved build-container-release "$(expected_comment)" release_reviewer
      ;;
    malformed-user)
      review=$(review_json approved build-container-release "$(expected_comment)")
      write_approvals "$(jq -c '.user = ["release-reviewer"]' <<<"$review")"
      ;;
  esac
  assert_result 1 "a current review with $reviewer_case shape is rejected" run_gate
done

for field in challenge image-digest release-tag run-id source-revision; do
  new_case
  comment=$(expected_comment)
  replacement64=$(printf '6%.0s' {1..64})
  case "$field" in
    challenge) comment=${comment/"$CHALLENGE"/"$replacement64"} ;;
    image-digest) comment=${comment/"$DIGEST"/"sha256:$replacement64"} ;;
    release-tag) comment=${comment/"$TAG"/build-container-v8} ;;
    run-id) comment=${comment/"\"run-id\":\"$RUN_ID\""/"\"run-id\":\"7\""} ;;
    source-revision)
      replacement40=$(printf '6%.0s' {1..40})
      comment=${comment/"$SOURCE"/"$replacement40"}
      ;;
  esac
  write_current_review approved build-container-release "$comment"
  assert_result 1 "a mismatched current $field is rejected" run_gate
done

new_case
write_run_body "{\"id\":$RUN_ID,\"run_attempt\":1}"
assert_result 1 'a mismatched current API run attempt is rejected' run_gate

new_case
earlier=$(expected_comment 2000-01-01T00:00:00Z)
earlier=${earlier/"\"run-attempt\":\"$RUN_ATTEMPT\""/"\"run-attempt\":\"1\""}
earlier_challenge=$(printf '7%.0s' {1..64})
earlier=${earlier/"$CHALLENGE"/"$earlier_challenge"}
write_approvals \
  "$(review_json rejected old-environment "$earlier" old-reviewer)" \
  "$(review_json approved build-container-release "$(expected_comment)")"
assert_result 0 'an earlier-attempt protocol record is inert' run_gate

new_case
malformed_earlier='edgezero-release-evidence-v1 {"run-attempt":"1","irrelevant":true}'
write_approvals \
  "$(review_json bypassed wrong-environment "$malformed_earlier" invalid_reviewer)" \
  "$(review_json approved build-container-release "$(expected_comment)")"
assert_result 0 'malformed details in a classifiable earlier attempt are inert' run_gate

new_case
future=$(expected_comment)
future=${future/"\"run-attempt\":\"$RUN_ATTEMPT\""/"\"run-attempt\":\"3\""}
write_approvals \
  "$(review_json approved build-container-release "$(expected_comment)")" \
  "$(review_json approved build-container-release "$future")"
assert_result 1 'a future-attempt protocol predeclaration is rejected' run_gate

new_case
write_approvals "$(review_json approved build-container-release 'ordinary approval')"
assert_result 1 'missing current protocol review is rejected' run_gate

new_case
earlier_only=$(expected_comment)
earlier_only=${earlier_only/"\"run-attempt\":\"$RUN_ATTEMPT\""/"\"run-attempt\":\"1\""}
write_approvals "$(review_json approved build-container-release "$earlier_only")"
assert_result 1 'earlier-attempt-only evidence cannot satisfy the current attempt' run_gate

for field in id run_attempt; do
  if [[ "$field" == id ]]; then
    values=('"2"' 1.5 0 18446744073709551616)
  else
    values=('"2"' 1.5 0 4294967296)
  fi
  for value in "${values[@]}"; do
    new_case
    if [[ "$field" == id ]]; then
      write_run_body "{\"id\":$value,\"run_attempt\":$RUN_ATTEMPT}"
    else
      write_run_body "{\"id\":$RUN_ID,\"run_attempt\":$value}"
    fi
    assert_result 1 "API $field rejects non-integer, zero, or out-of-range value $value" run_gate
  done
done

for endpoint_case in run-status run-version run-media run-charset run-malformed run-multiple approvals-status approvals-version approvals-media; do
  new_case
  case "$endpoint_case" in
    run-status) set_metadata run 302 ;;
    run-version) set_metadata run 200 2022-11-28 ;;
    run-media) set_metadata run 200 2026-03-10 text/json ;;
    run-charset) set_metadata run 200 2026-03-10 'application/json; charset=iso-8859-1' ;;
    run-malformed) printf '{' >"$FAKE_BIN/run.body" ;;
    run-multiple) printf '{} {}' >"$FAKE_BIN/run.body" ;;
    approvals-status) set_metadata approvals 500 ;;
    approvals-version) set_metadata approvals 200 2022-11-28 ;;
    approvals-media) set_metadata approvals 200 2026-03-10 text/json ;;
  esac
  assert_result 1 "$endpoint_case response is rejected" run_gate
done

for approvals_shape in malformed multiple; do
  new_case
  if [[ "$approvals_shape" == malformed ]]; then
    printf '[' >"$FAKE_BIN/approvals.body"
  else
    printf '[] []' >"$FAKE_BIN/approvals.body"
  fi
  assert_result 1 "$approvals_shape approvals JSON is rejected" run_gate
done

new_case
touch "$FAKE_BIN/run.transport-failure"
assert_result 1 'a curl transport failure is terminal without output' run_gate
if [[ $(curl_calls) == 1 ]]; then
  ok 'a curl transport failure is not retried'
else
  no 'a curl transport failure is not retried'
fi
if [[ $(<"$CASE_ROOT/stderr") == '::error::current run request failed' ]]; then
  ok 'a curl transport failure emits only sanitized stderr'
else
  cat "$CASE_ROOT/stderr" >&2
  no 'a curl transport failure emits only sanitized stderr'
fi

new_case
response_secret='server-response-must-not-leak'
write_run_body "{\"id\":\"$response_secret\",\"run_attempt\":$RUN_ATTEMPT}"
assert_result 1 'a malformed response body is rejected without output' run_gate
if ! grep -Fq "$response_secret" "$CASE_ROOT/stdout" &&
  ! grep -Fq "$response_secret" "$CASE_ROOT/stderr"; then
  ok 'response body content is absent from diagnostics'
else
  no 'response body content is absent from diagnostics'
fi

new_case
printf '200\n2026-03-10\napplication/json\nextra' >"$FAKE_BIN/run.raw-metadata"
assert_result 1 'malformed response metadata cardinality is rejected' run_gate

new_case
touch "$FAKE_BIN/fail-metadata-mktemp"
assert_result 1 'metadata tempfile creation failure is rejected' run_gate
body_temp=$(sed -n '1p' "$FAKE_BIN/mktemp-created")
if [[ -n "$body_temp" && ! -e "$body_temp" ]]; then
  ok 'body tempfile is registered and cleaned before metadata tempfile creation'
else
  [[ -z "$body_temp" ]] || rm -f -- "$body_temp"
  no 'body tempfile is registered and cleaned before metadata tempfile creation'
fi
assert_no_curl 'metadata tempfile creation failure occurs before curl'

for reviewed_at in 2026-09-10T11:44:59Z 2026-09-10T12:00:01Z \
  2026-02-30T12:00:00Z 2026-09-10T11:55:00+00:00 2026-09-10T11:55:00.000Z; do
  new_case
  write_current_review approved build-container-release "$(expected_comment "$reviewed_at")"
  assert_result 1 "review timestamp $reviewed_at is rejected" run_gate
done

new_case
write_current_review approved build-container-release "$(expected_comment 2026-09-10T11:45:00Z)"
assert_result 0 'a review exactly 900 seconds old is accepted' run_gate

new_case
assert_result 0 'initial publication for no-replace test succeeds' run_gate
printf '%s' "$expected_output" >"$CASE_ROOT/preserved"
status=0
run_gate >"$CASE_ROOT/second-stdout" 2>"$CASE_ROOT/second-stderr" || status=$?
if [[ "$status" -eq 1 && ! -s "$CASE_ROOT/second-stdout" ]] &&
  cmp -s "$CASE_ROOT/preserved" "$APPROVAL_OUT" && [[ $(curl_calls) == 2 ]]; then
  ok 'an existing output is never replaced and blocks before curl'
else
  cat "$CASE_ROOT/second-stdout" "$CASE_ROOT/second-stderr" >&2
  no 'an existing output is never replaced and blocks before curl'
fi

new_case
APPROVAL_OUT="$GATE_ROOT/approval.json"
assert_result 1 'an output path inside a repository is rejected' run_gate
assert_no_curl 'repository-contained output is rejected before curl'

new_case
chmod 0755 "$OUTPUT_ROOT"
assert_result 1 'a non-private output directory is rejected' run_gate
assert_no_curl 'non-private output directory is rejected before curl'
chmod 0700 "$OUTPUT_ROOT"

for bad_token in $'line\nbreak' 'double"quote' 'back\slash'; do
  new_case
  TOKEN_VALUE=$bad_token
  assert_result 1 'an unsafe token encoding is rejected' run_gate
  assert_no_curl 'unsafe token encoding is rejected before curl'
done

new_case
TOKEN_VALUE=$'bad\ntoken'
printf 'dirty\n' >"$GATE_ROOT/dirty"
assert_result 1 'checkout validation precedes credential validation' run_gate
assert_no_curl 'a dirty checkout with a bad token does not call curl'
rm "$GATE_ROOT/dirty"

new_case
TOKEN_VALUE=$'bad\ntoken'
git -C "$GATE_ROOT" switch -q main
assert_result 1 'an attached gate checkout is rejected before credential access' run_gate
assert_no_curl 'an attached gate checkout does not call curl'
git -C "$GATE_ROOT" checkout -q --detach "$G"

new_case
TOKEN_VALUE=$'bad\ntoken'
CLI_GATE_SHA=$(printf 'f%.0s' {1..40})
assert_result 1 'a supplied gate SHA different from HEAD is rejected before credential access' run_gate
assert_no_curl 'a wrong supplied gate SHA does not call curl'

new_case
TOKEN_VALUE=$'bad\ntoken'
touch "$FAKE_BIN/fail-git-status"
assert_result 1 'a failed gate cleanliness probe is rejected before credential use' run_gate
if grep -Fq 'cannot inspect gate checkout status' "$CASE_ROOT/stderr"; then
  ok 'a failed gate cleanliness probe cannot masquerade as a clean checkout'
else
  cat "$CASE_ROOT/stderr" >&2
  no 'a failed gate cleanliness probe cannot masquerade as a clean checkout'
fi
assert_no_curl 'a failed gate cleanliness probe does not call curl'

new_case
cat >"$CASE_ROOT/shell-signal-hook" <<'SH'
set -T
trap '
  if [[ ${BASH_COMMAND:-} == GATE_ROOT= && ! -e "$SIGNAL_FIXTURE/shell-blocked" ]]; then
    : >"$SIGNAL_FIXTURE/shell-blocked"
    while [[ ! -e "$SIGNAL_FIXTURE/release-shell" ]]; do :; done
  fi
' DEBUG
SH
start_gate "$CASE_ROOT/shell-signal-hook"
if wait_for_marker "$CASE_ROOT/shell-blocked" "$SIGNAL_GATE_PID"; then
  kill -HUP "$SIGNAL_GATE_PID"
fi
wait_for_signaled_gate "$SIGNAL_GATE_PID"
assert_signal_cleanup 129 'HUP during shell-only processing exits nonzero and cleans all output'

new_case
touch "$FAKE_BIN/run.block"
start_gate /dev/null
if wait_for_marker "$FAKE_BIN/curl-blocked" "$SIGNAL_GATE_PID"; then
  CURL_CHILD_PID=$(<"$FAKE_BIN/curl-child-pid")
  kill -TERM "$SIGNAL_GATE_PID"
  # Bash defers its trap while waiting for the foreground child. Terminating
  # the fake transport releases that wait so the queued TERM trap can run.
  kill -TERM "$CURL_CHILD_PID"
else
  CURL_CHILD_PID=
fi
wait_for_signaled_gate "$SIGNAL_GATE_PID" "$FAKE_BIN/curl-child-pid"
assert_signal_cleanup 143 'TERM while curl is blocked exits nonzero and cleans all output'
if [[ -n "$CURL_CHILD_PID" ]] && ! kill -0 "$CURL_CHILD_PID" 2>/dev/null; then
  ok 'the blocked curl child does not survive gate termination'
else
  [[ -z "$CURL_CHILD_PID" ]] || kill -KILL "$CURL_CHILD_PID" 2>/dev/null || true
  no 'the blocked curl child does not survive gate termination'
fi

if ((fail)); then
  printf '\n%d passed, %d failed\n' "$pass" "$fail" >&2
  exit 1
fi
printf '\n%d passed, %d failed\n' "$pass" "$fail"
