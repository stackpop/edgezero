#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REAL_VERIFY="$DIR/../../../docker/build-app-cli/verify-build-container-publication.sh"
REAL_CHECK="$DIR/../../../docker/build-app-cli/check-image-pin.sh"
REAL_BASH=$(command -v bash)
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
REVIEWED_AT=2020-01-02T03:04:05Z
TOKEN_VALUE='fixture-token-value'
API_ROOT=https://api.github.com/repos/stackpop/edgezero/actions/runs

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
SUBJECT_ROOT="$WORK/subject"
RECORD_ROOT="$WORK/records"
FAKE_BIN="$WORK/fake-bin"
mkdir -p "$GATE_ROOT/.github/docker/build-app-cli" \
  "$SUBJECT_ROOT/.github/docker/build-app-cli" "$RECORD_ROOT" "$FAKE_BIN"
cp "$REAL_VERIFY" "$GATE_ROOT/.github/docker/build-app-cli/verify-build-container-publication.sh"
cp "$REAL_CHECK" "$GATE_ROOT/.github/docker/build-app-cli/check-image-pin.sh"
chmod 0755 "$GATE_ROOT/.github/docker/build-app-cli/verify-build-container-publication.sh" \
  "$GATE_ROOT/.github/docker/build-app-cli/check-image-pin.sh"

git -C "$GATE_ROOT" init -q -b main
git -C "$GATE_ROOT" config user.name fixture
git -C "$GATE_ROOT" config user.email fixture@example.invalid
git -C "$GATE_ROOT" add .
git -C "$GATE_ROOT" commit -q -m gate-base
printf '%s\n' gate >"$GATE_ROOT/gate-marker"
git -C "$GATE_ROOT" add gate-marker
git -C "$GATE_ROOT" commit -q -m gate
G=$(git -C "$GATE_ROOT" rev-parse HEAD)
git -C "$GATE_ROOT" checkout -q --detach "$G"

IMAGE_CONTENT="{\"digest\":\"$DIGEST\",\"image-source-revision\":\"$SOURCE\",\"provenance-protocol\":1,\"repository\":\"ghcr.io/stackpop/edgezero-build-app-cli\",\"tag\":\"$TAG\"}"
EVIDENCE_CONTENT="{\"approval-challenge\":\"$CHALLENGE\",\"approver-login\":\"release-reviewer\",\"image-digest\":\"$DIGEST\",\"release-tag\":\"$TAG\",\"reviewed-at\":\"$REVIEWED_AT\",\"run-attempt\":\"$RUN_ATTEMPT\",\"run-id\":\"$RUN_ID\",\"schema-version\":1,\"screenshot-sha256\":\"$SCREENSHOT\",\"source-revision\":\"$SOURCE\"}"
printf '%s' "$IMAGE_CONTENT" >"$SUBJECT_ROOT/.github/docker/build-app-cli/image.json"
printf '%s' "$EVIDENCE_CONTENT" >"$SUBJECT_ROOT/.github/docker/build-app-cli/image-release-evidence.json"
git -C "$SUBJECT_ROOT" init -q -b main
git -C "$SUBJECT_ROOT" config user.name fixture
git -C "$SUBJECT_ROOT" config user.email fixture@example.invalid
git -C "$SUBJECT_ROOT" add .
git -C "$SUBJECT_ROOT" commit -q -m subject
T=$(git -C "$SUBJECT_ROOT" rev-parse HEAD)
git -C "$SUBJECT_ROOT" checkout -q --detach "$T"

IMAGE_JSON="$RECORD_ROOT/image.json"
EVIDENCE_JSON="$RECORD_ROOT/image-release-evidence.json"
VERIFY="$GATE_ROOT/.github/docker/build-app-cli/verify-build-container-publication.sh"

cat >"$FAKE_BIN/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=${0%/*}
[[ "$fixture" != "$0" ]] || fixture=.
fixture=$(cd -- "$fixture" && pwd)
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
    --output) output=$2; shift 2 ;;
    http*) url=$1; shift ;;
    *) shift ;;
  esac
done
[[ -n "$output" && -n "$url" ]]
case "$url" in
  */approvals) endpoint=approvals ;;
  */jobs\?per_page=100\&page=1) endpoint=jobs ;;
  *) endpoint=run ;;
esac
endpoint_count=0
[[ ! -f "$fixture/$endpoint-count" ]] || endpoint_count=$(<"$fixture/$endpoint-count")
endpoint_count=$((endpoint_count + 1))
printf '%s' "$endpoint_count" >"$fixture/$endpoint-count"
printf 'curl %s\n' "$url" >>"$fixture/events"
if [[ -f "$fixture/$endpoint.transport-failure" ]]; then
  printf '%s\n' 'curl: fixture-token-value secret-response-body' >&2
  exit 28
fi
body="$fixture/$endpoint.body"
[[ ! -f "$fixture/$endpoint.$endpoint_count.body" ]] || body="$fixture/$endpoint.$endpoint_count.body"
cat "$body" >"$output"
if [[ -f "$fixture/$endpoint.raw-metadata" ]]; then
  cat "$fixture/$endpoint.raw-metadata"
else
  printf '%s\n%s\n%s\nlink=%s' \
    "$(<"$fixture/$endpoint.status")" \
    "$(<"$fixture/$endpoint.version")" \
    "$(<"$fixture/$endpoint.media")" \
    "$(<"$fixture/$endpoint.link")"
fi
SH
chmod 0755 "$FAKE_BIN/curl"

cat >"$FAKE_BIN/sleep" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=$(cd -- "$(dirname -- "$0")" && pwd)
[[ -z "${GITHUB_TOKEN+x}${GH_TOKEN+x}${HOME+x}${AMBIENT_SECRET+x}" ]]
printf '%s\n' "$*" >>"$fixture/sleeps"
printf 'sleep %s\n' "$*" >>"$fixture/events"
SH
chmod 0755 "$FAKE_BIN/sleep"

cat >"$FAKE_BIN/tool-probe" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=${0%/*}
[[ "$fixture" != "$0" ]] || fixture=.
fixture=$(cd -- "$fixture" && pwd)
tool=${0##*/}
while IFS='=' read -r _ value; do
  if [[ "$value" == fixture-token-value ]]; then
    : >"$fixture/credential-env-leak"
    break
  fi
done < <(/usr/bin/env)
if [[ "$tool" == git && "${GIT_NO_LAZY_FETCH:-}" != 1 ]]; then
  : >"$fixture/lazy-fetch-enabled"
fi
PATH=${PATH#*:}
export PATH
hash -r
real_tool=$(command -v "$tool")
[[ "$real_tool" == /* ]]
exec "$real_tool" "$@"
SH
chmod 0755 "$FAKE_BIN/tool-probe"
for tool in git jq awk cmp dirname mktemp rm; do
  ln -s tool-probe "$FAKE_BIN/$tool"
done

write_run() {
  local path=$1 status=${2:-completed} conclusion=${3:-'"success"'}
  local id=${4:-$RUN_ID} attempt=${5:-$RUN_ATTEMPT} event=${6:-push}
  local workflow_path=${7:-.github/workflows/publish-build-container.yml@$TAG}
  local head_sha=${8:-$SOURCE} head_branch=${9:-$TAG}
  printf '%s' "{\"id\":$id,\"run_attempt\":$attempt,\"event\":\"$event\",\"path\":\"$workflow_path\",\"head_sha\":\"$head_sha\",\"head_branch\":\"$head_branch\",\"status\":\"$status\",\"conclusion\":$conclusion}" >"$path"
}

write_jobs() {
  local path=$1
  printf '%s' "{\"total_count\":2,\"jobs\":[{\"name\":\"build-and-verify\",\"status\":\"completed\",\"conclusion\":\"success\",\"head_sha\":\"$SOURCE\",\"run_attempt\":$RUN_ATTEMPT,\"steps\":[{\"name\":\"checkout\",\"conclusion\":\"success\"},{\"name\":\"assert-exact-publisher-context\",\"conclusion\":\"success\"}]},{\"name\":\"update-pin\",\"status\":\"completed\",\"conclusion\":\"success\",\"head_sha\":\"$SOURCE\",\"run_attempt\":$RUN_ATTEMPT,\"steps\":[{\"name\":\"assert-exact-publisher-context\",\"conclusion\":\"success\"},{\"name\":\"update\",\"conclusion\":\"success\"}]}]}" >"$path"
}

protocol_comment() {
  local attempt=${1:-$RUN_ATTEMPT} run_id=${2:-$RUN_ID} challenge=${3:-$CHALLENGE}
  local digest=${4:-$DIGEST} tag=${5:-$TAG} reviewed=${6:-$REVIEWED_AT}
  local screenshot=${7:-$SCREENSHOT} source=${8:-$SOURCE}
  printf '%s' "edgezero-release-evidence-v1 {\"challenge\":\"$challenge\",\"image-digest\":\"$digest\",\"png-sha256\":\"$screenshot\",\"release-tag\":\"$tag\",\"reviewed-at\":\"$reviewed\",\"run-attempt\":\"$attempt\",\"run-id\":\"$run_id\",\"source-revision\":\"$source\"}"
}

review_json() {
  local state=$1 environment=$2 comment=$3 login=${4:-release-reviewer}
  jq -cn --arg state "$state" --arg environment "$environment" \
    --arg comment "$comment" --arg login "$login" \
    '{state:$state,environments:[{name:$environment}],user:{login:$login},comment:$comment}'
}

reviewer_only_json() {
  local state=$1 environment=$2 comment=$3 login=${4:-release-reviewer}
  jq -cn --arg state "$state" --arg environment "$environment" \
    --arg comment "$comment" --arg login "$login" \
    '{state:$state,environments:[{name:$environment}],reviewer:{login:$login},comment:$comment}'
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
  local comment=${3:-$(protocol_comment)} login=${4:-release-reviewer}
  write_approvals "$(review_json "$state" "$environment" "$comment" "$login")"
}

reset_repositories() {
  git -C "$GATE_ROOT" config --unset core.sparseCheckout 2>/dev/null || true
  git -C "$SUBJECT_ROOT" config --unset core.sparseCheckout 2>/dev/null || true
  git -C "$GATE_ROOT" config --unset-all extensions.partialClone 2>/dev/null || true
  git -C "$SUBJECT_ROOT" config --unset-all extensions.partialClone 2>/dev/null || true
  git -C "$GATE_ROOT" config --unset-all remote.origin.promisor 2>/dev/null || true
  git -C "$SUBJECT_ROOT" config --unset-all remote.origin.promisor 2>/dev/null || true
  git -C "$GATE_ROOT" config --unset-all remote.origin.partialCloneFilter 2>/dev/null || true
  git -C "$SUBJECT_ROOT" config --unset-all remote.origin.partialCloneFilter 2>/dev/null || true
  rm -f "$GATE_ROOT/.git/objects/info/alternates" "$SUBJECT_ROOT/.git/objects/info/alternates"
  git -C "$GATE_ROOT" replace -d "$G" >/dev/null 2>&1 || true
  rm -f "$GATE_ROOT/.git/info/grafts" "$SUBJECT_ROOT/.git/info/grafts"
  git -C "$GATE_ROOT" reset --hard -q "$G"
  git -C "$GATE_ROOT" clean -fdq
  git -C "$GATE_ROOT" checkout -q --detach "$G"
  git -C "$SUBJECT_ROOT" reset --hard -q "$T"
  git -C "$SUBJECT_ROOT" clean -fdq
  git -C "$SUBJECT_ROOT" checkout -q --detach "$T"
  git -C "$SUBJECT_ROOT" show "$T:.github/docker/build-app-cli/image.json" >"$IMAGE_JSON"
  git -C "$SUBJECT_ROOT" show "$T:.github/docker/build-app-cli/image-release-evidence.json" >"$EVIDENCE_JSON"
}

reset_api() {
  rm -f "$FAKE_BIN"/{args-*,config-*,curl-count,run-count,jobs-count,approvals-count,events,sleeps} \
    "$FAKE_BIN"/{run,jobs,approvals}.{transport-failure,raw-metadata} \
    "$FAKE_BIN"/run.*.body "$FAKE_BIN"/{credential-env-leak,lazy-fetch-enabled}
  write_run "$FAKE_BIN/run.body"
  write_jobs "$FAKE_BIN/jobs.body"
  write_current_review
  local endpoint
  for endpoint in run jobs approvals; do
    printf '200' >"$FAKE_BIN/$endpoint.status"
    printf '2026-03-10' >"$FAKE_BIN/$endpoint.version"
    printf 'application/json; charset=utf-8' >"$FAKE_BIN/$endpoint.media"
    : >"$FAKE_BIN/$endpoint.link"
  done
}

reset_case() {
  case_number=$((case_number + 1))
  reset_repositories
  reset_api
  : >"$WORK/stdout"
  : >"$WORK/stderr"
}

default_args() {
  printf '%s\0' \
    --gate-root "$GATE_ROOT" --subject-root "$SUBJECT_ROOT" \
    --gate-sha "$G" --candidate-sha "$T" \
    --image-json "$IMAGE_JSON" --evidence-json "$EVIDENCE_JSON"
}

invoke() {
  local verifier=$1
  shift
  if env PATH="$FAKE_BIN:$PATH" GITHUB_TOKEN="$TOKEN_VALUE" TOKEN=exported-token-slot \
    AMBIENT_SECRET=must-not-propagate bash "$verifier" "$@" \
    >"$WORK/stdout" 2>"$WORK/stderr"; then
    CAPTURE_STATUS=0
  else
    CAPTURE_STATUS=$?
  fi
}

invoke_with_hostile_shell_options() {
  local -a args=()
  while IFS= read -r -d '' arg; do args+=("$arg"); done < <(default_args)
  if env PATH="$FAKE_BIN:$PATH" GITHUB_TOKEN="$TOKEN_VALUE" TOKEN=exported-token-slot \
    AMBIENT_SECRET=must-not-propagate bash -x -a "$VERIFY" "${args[@]}" \
    >"$WORK/stdout" 2>"$WORK/stderr"; then
    CAPTURE_STATUS=0
  else
    CAPTURE_STATUS=$?
  fi
}

invoke_with_missing_tool() {
  local missing=$1 tool path missing_bin
  local -a args=()
  missing_bin="$WORK/missing-$missing"
  mkdir -p "$missing_bin"
  for tool in env git jq curl sleep mktemp cmp awk rm dirname bash; do
    [[ "$tool" == "$missing" ]] && continue
    path=$(command -v "$tool")
    ln -s "$path" "$missing_bin/$tool"
  done
  while IFS= read -r -d '' arg; do args+=("$arg"); done < <(default_args)
  if env PATH="$missing_bin" GITHUB_TOKEN="$TOKEN_VALUE" TOKEN=exported-token-slot \
    "$REAL_BASH" "$VERIFY" "${args[@]}" >"$WORK/stdout" 2>"$WORK/stderr"; then
    CAPTURE_STATUS=0
  else
    CAPTURE_STATUS=$?
  fi
}

invoke_default() {
  local -a args=()
  while IFS= read -r -d '' arg; do args+=("$arg"); done < <(default_args)
  invoke "$VERIFY" "${args[@]}"
}

assert_status() {
  local description=$1 expected=$2
  if [[ "$CAPTURE_STATUS" -eq "$expected" ]]; then
    ok "$description"
  else
    no "$description"
    printf '    expected status: %s, actual status: %s\n' "$expected" "$CAPTURE_STATUS" >&2
    sed -n '1p' "$WORK/stderr" >&2
  fi
}

assert_silent_success() {
  local description=$1
  if [[ "$CAPTURE_STATUS" -eq 0 && ! -s "$WORK/stdout" && ! -s "$WORK/stderr" ]]; then
    ok "$description"
  else
    no "$description"
    printf '    actual status: %s\n' "$CAPTURE_STATUS" >&2
    sed -n '1p' "$WORK/stderr" >&2
  fi
}

assert_no_network() {
  local description=$1
  if [[ ! -e "$FAKE_BIN/curl-count" ]]; then ok "$description"; else no "$description"; fi
}

assert_eq() {
  local description=$1 expected=$2 actual=$3
  if [[ "$actual" == "$expected" ]]; then ok "$description"; else no "$description"; fi
}

file_value_or_zero() {
  local path=$1
  if [[ -f "$path" ]]; then cat "$path"; else printf 0; fi
}

line_count_or_zero() {
  local path=$1
  if [[ -f "$path" ]]; then wc -l <"$path" | tr -d '[:space:]'; else printf 0; fi
}

assert_complete_curl_request() {
  local description=$1 ordinal=$2 expected_url=$3 line normalized expected config
  local -a args=()
  if [[ ! -f "$FAKE_BIN/args-$ordinal" || ! -f "$FAKE_BIN/config-$ordinal" ]]; then
    no "$description"
    return
  fi
  while IFS= read -r line || [[ -n "$line" ]]; do args+=("$line"); done <"$FAKE_BIN/args-$ordinal"
  if [[ "${#args[@]}" -ne 18 ]]; then
    no "$description"
    return
  fi
  [[ "${args[14]}" == "$RECORD_ROOT/.edgezero-publication-body."?????? ]] || {
    no "$description"
    return
  }
  args[14]='<response-body>'
  normalized=$(printf '%s\n' "${args[@]}")
  expected="--disable
--silent
--show-error
--connect-timeout
10
--max-time
30
--max-redirs
0
--request
GET
--config
-
--output
<response-body>
--write-out
%{http_code}\\n%header{x-github-api-version-selected}\\n%header{content-type}\\nlink=%header{link}
$expected_url"
  config="header = \"Accept: application/vnd.github+json\"
header = \"X-GitHub-Api-Version: 2026-03-10\"
header = \"User-Agent: edgezero-build-container-gate/1\"
header = \"Authorization: Bearer $TOKEN_VALUE\""
  if [[ "$normalized" == "$expected" && "$(<"$FAKE_BIN/config-$ordinal")" == "$config" ]]; then
    ok "$description"
  else
    no "$description"
  fi
}

invoke_default_without_token() {
  local -a args=()
  while IFS= read -r -d '' arg; do args+=("$arg"); done < <(default_args)
  if env -u GITHUB_TOKEN PATH="$FAKE_BIN:$PATH" bash "$VERIFY" "${args[@]}" \
    >"$WORK/stdout" 2>"$WORK/stderr"; then
    CAPTURE_STATUS=0
  else
    CAPTURE_STATUS=$?
  fi
}

echo "== build container publication verifier =="

reset_case
invoke_default
assert_silent_success "a valid archived publication is accepted silently"
assert_eq "success performs exactly four API reads" 4 "$(file_value_or_zero "$FAKE_BIN/curl-count")"
if [[ ! -e "$FAKE_BIN/credential-env-leak" ]]; then
  ok "the token is absent from every verifier child environment"
else
  no "the token is absent from every verifier child environment"
fi
if [[ ! -e "$FAKE_BIN/lazy-fetch-enabled" ]]; then
  ok "every Git read disables lazy object fetching"
else
  no "every Git read disables lazy object fetching"
fi

reset_case
invoke_with_hostile_shell_options
assert_status "inherited xtrace and allexport do not change success" 0
if ! rg -F -q "$TOKEN_VALUE" "$WORK/stdout" "$WORK/stderr" &&
  [[ ! -e "$FAKE_BIN/credential-env-leak" ]]; then
  ok "inherited xtrace and allexport cannot expose the token"
else
  no "inherited xtrace and allexport cannot expose the token"
fi

reset_case
invoke "$VERIFY" --gate-root "$GATE_ROOT"
assert_status "missing flags return usage status 2" 2
assert_no_network "missing flags fail before token or network"

reset_case
invoke "$VERIFY" --unknown value
assert_status "unknown flags return usage status 2" 2
assert_no_network "unknown flags fail before token or network"

reset_case
invoke "$VERIFY" --gate-root "$GATE_ROOT" --gate-root "$GATE_ROOT" \
  --subject-root "$SUBJECT_ROOT" --gate-sha "$G" --candidate-sha "$T" \
  --image-json "$IMAGE_JSON" --evidence-json "$EVIDENCE_JSON"
assert_status "duplicate flags return usage status 2" 2
assert_no_network "duplicate flags fail before token or network"

reset_case
invoke "$VERIFY" --gate-root '' --subject-root "$SUBJECT_ROOT" --gate-sha "$G" \
  --candidate-sha "$T" --image-json "$IMAGE_JSON" --evidence-json "$EVIDENCE_JSON"
assert_status "empty flag values return usage status 2" 2
assert_no_network "empty values fail before token or network"

for missing_tool in dirname bash; do
  reset_case
  invoke_with_missing_tool "$missing_tool"
  assert_status "missing $missing_tool returns tooling status 2" 2
  assert_no_network "missing $missing_tool fails before network"
done

reset_case
invoke_default_without_token
assert_status "an absent GITHUB_TOKEN is rejected" 1
assert_no_network "an absent token causes no request"

reset_case
printf '\n' >>"$IMAGE_JSON"
invoke_default
assert_status "an extracted image differing from its exact blob is rejected" 1
assert_no_network "image blob mismatch is pre-network"

reset_case
printf '\n' >>"$EVIDENCE_JSON"
invoke_default
assert_status "extracted evidence differing from its exact blob is rejected" 1
assert_no_network "evidence blob mismatch is pre-network"

reset_case
chmod 0755 "$SUBJECT_ROOT/.github/docker/build-app-cli/image.json"
git -C "$SUBJECT_ROOT" add .github/docker/build-app-cli/image.json
git -C "$SUBJECT_ROOT" commit -q -m executable-image-record
EXECUTABLE_IMAGE_T=$(git -C "$SUBJECT_ROOT" rev-parse HEAD)
git -C "$SUBJECT_ROOT" checkout -q --detach "$EXECUTABLE_IMAGE_T"
git -C "$SUBJECT_ROOT" show \
  "$EXECUTABLE_IMAGE_T:.github/docker/build-app-cli/image.json" >"$IMAGE_JSON"
invoke "$VERIFY" --gate-root "$GATE_ROOT" --subject-root "$SUBJECT_ROOT" \
  --gate-sha "$G" --candidate-sha "$EXECUTABLE_IMAGE_T" --image-json "$IMAGE_JSON" \
  --evidence-json "$EVIDENCE_JSON"
assert_status "an executable candidate image record is rejected" 1
assert_no_network "executable image record rejection is pre-network"

reset_case
chmod 0755 "$SUBJECT_ROOT/.github/docker/build-app-cli/image-release-evidence.json"
git -C "$SUBJECT_ROOT" add .github/docker/build-app-cli/image-release-evidence.json
git -C "$SUBJECT_ROOT" commit -q -m executable-evidence-record
EXECUTABLE_EVIDENCE_T=$(git -C "$SUBJECT_ROOT" rev-parse HEAD)
git -C "$SUBJECT_ROOT" checkout -q --detach "$EXECUTABLE_EVIDENCE_T"
git -C "$SUBJECT_ROOT" show \
  "$EXECUTABLE_EVIDENCE_T:.github/docker/build-app-cli/image-release-evidence.json" \
  >"$EVIDENCE_JSON"
invoke "$VERIFY" --gate-root "$GATE_ROOT" --subject-root "$SUBJECT_ROOT" \
  --gate-sha "$G" --candidate-sha "$EXECUTABLE_EVIDENCE_T" --image-json "$IMAGE_JSON" \
  --evidence-json "$EVIDENCE_JSON"
assert_status "an executable candidate evidence record is rejected" 1
assert_no_network "executable evidence record rejection is pre-network"

reset_case
ln -s "$IMAGE_JSON" "$RECORD_ROOT/image-link.json"
invoke "$VERIFY" --gate-root "$GATE_ROOT" --subject-root "$SUBJECT_ROOT" \
  --gate-sha "$G" --candidate-sha "$T" --image-json "$RECORD_ROOT/image-link.json" \
  --evidence-json "$EVIDENCE_JSON"
assert_status "a symlink input record is rejected" 1
assert_no_network "symlink input rejection is pre-network"
rm -f "$RECORD_ROOT/image-link.json"

reset_case
invoke "$VERIFY" --gate-root "$GATE_ROOT" --subject-root "$SUBJECT_ROOT" \
  --gate-sha "$G" --candidate-sha "$T" --image-json records/image.json \
  --evidence-json "$EVIDENCE_JSON"
assert_status "a non-absolute input record is rejected" 1
assert_no_network "non-absolute input rejection is pre-network"

reset_case
invoke "$VERIFY" --gate-root "$GATE_ROOT" --subject-root "$SUBJECT_ROOT" \
  --gate-sha "$G" --candidate-sha "$T" \
  --image-json "$SUBJECT_ROOT/.github/docker/build-app-cli/image.json" \
  --evidence-json "$EVIDENCE_JSON"
assert_status "an input record inside a repository is rejected" 1
assert_no_network "inside-repository input rejection is pre-network"

reset_case
printf '%s' '{"bad":true}' >"$SUBJECT_ROOT/.github/docker/build-app-cli/image.json"
git -C "$SUBJECT_ROOT" add .github/docker/build-app-cli/image.json
git -C "$SUBJECT_ROOT" commit -q -m invalid-record
BAD_T=$(git -C "$SUBJECT_ROOT" rev-parse HEAD)
git -C "$SUBJECT_ROOT" checkout -q --detach "$BAD_T"
git -C "$SUBJECT_ROOT" show "$BAD_T:.github/docker/build-app-cli/image.json" >"$IMAGE_JSON"
invoke "$VERIFY" --gate-root "$GATE_ROOT" --subject-root "$SUBJECT_ROOT" \
  --gate-sha "$G" --candidate-sha "$BAD_T" --image-json "$IMAGE_JSON" \
  --evidence-json "$EVIDENCE_JSON"
assert_status "an invalid exact candidate record pair is rejected" 1
assert_no_network "record validation runs before network"

reset_case
printf x >"$GATE_ROOT/dirty"
invoke_default
assert_status "untracked gate state is rejected" 1
assert_no_network "dirty gate rejection is pre-network"

reset_case
printf x >>"$SUBJECT_ROOT/.github/docker/build-app-cli/image.json"
invoke_default
assert_status "dirty tracked subject state is rejected" 1
assert_no_network "dirty subject rejection is pre-network"

reset_case
printf x >>"$SUBJECT_ROOT/.github/docker/build-app-cli/image.json"
git -C "$SUBJECT_ROOT" add .github/docker/build-app-cli/image.json
invoke_default
assert_status "dirty subject index state is rejected" 1
assert_no_network "dirty index rejection is pre-network"

reset_case
git -C "$GATE_ROOT" checkout -q main
invoke_default
assert_status "an attached gate checkout is rejected" 1
assert_no_network "attached checkout rejection is pre-network"

reset_case
git -C "$SUBJECT_ROOT" config core.sparseCheckout true
invoke_default
assert_status "a sparse subject checkout is rejected" 1
assert_no_network "sparse checkout rejection is pre-network"

reset_case
git -C "$GATE_ROOT" replace "$G" "$G^" >/dev/null 2>&1 || true
invoke_default
assert_status "gate replacement refs are rejected" 1
assert_no_network "replacement-ref rejection is pre-network"

reset_case
: >"$GATE_ROOT/.git/info/grafts"
invoke_default
assert_status "legacy gate grafts are rejected" 1
assert_no_network "graft rejection is pre-network"

reset_case
invoke "$VERIFY" --gate-root "$GATE_ROOT/.github" --subject-root "$SUBJECT_ROOT" \
  --gate-sha "$G" --candidate-sha "$T" --image-json "$IMAGE_JSON" \
  --evidence-json "$EVIDENCE_JSON"
assert_status "a non-top-level gate root is rejected" 1
assert_no_network "top-level rejection is pre-network"

reset_case
ln -s "$GATE_ROOT" "$WORK/gate-link"
invoke "$WORK/gate-link/.github/docker/build-app-cli/verify-build-container-publication.sh" \
  --gate-root "$WORK/gate-link" --subject-root "$SUBJECT_ROOT" --gate-sha "$G" \
  --candidate-sha "$T" --image-json "$IMAGE_JSON" --evidence-json "$EVIDENCE_JSON"
assert_status "a non-canonical gate root is rejected" 1
assert_no_network "canonical-root rejection is pre-network"
rm -f "$WORK/gate-link"

reset_case
invoke "$VERIFY" --gate-root "$GATE_ROOT" --subject-root "$GATE_ROOT" \
  --gate-sha "$G" --candidate-sha "$G" --image-json "$IMAGE_JSON" \
  --evidence-json "$EVIDENCE_JSON"
assert_status "gate and subject roots must be separate repositories" 1
assert_no_network "same-repository rejection is pre-network"

reset_case
git -C "$SUBJECT_ROOT" update-index --add --cacheinfo "160000,$G,fixture-submodule"
git -C "$SUBJECT_ROOT" commit -q -m gitlink
SUBMODULE_T=$(git -C "$SUBJECT_ROOT" rev-parse HEAD)
git -C "$SUBJECT_ROOT" checkout -q --detach "$SUBMODULE_T"
invoke "$VERIFY" --gate-root "$GATE_ROOT" --subject-root "$SUBJECT_ROOT" \
  --gate-sha "$G" --candidate-sha "$SUBMODULE_T" --image-json "$IMAGE_JSON" \
  --evidence-json "$EVIDENCE_JSON"
assert_status "committed submodule state is rejected" 1
assert_no_network "submodule rejection is pre-network"

reset_case
SHALLOW_GATE="$WORK/shallow-gate-$case_number"
git clone -q --depth 1 "file://$GATE_ROOT" "$SHALLOW_GATE"
git -C "$SHALLOW_GATE" checkout -q --detach "$G"
invoke "$SHALLOW_GATE/.github/docker/build-app-cli/verify-build-container-publication.sh" \
  --gate-root "$SHALLOW_GATE" --subject-root "$SUBJECT_ROOT" --gate-sha "$G" \
  --candidate-sha "$T" --image-json "$IMAGE_JSON" --evidence-json "$EVIDENCE_JSON"
assert_status "a shallow gate repository is rejected" 1
assert_no_network "shallow repository rejection is pre-network"

reset_case
git -C "$SUBJECT_ROOT" config extensions.partialClone origin
invoke_default
assert_status "partial-clone configuration is rejected" 1
assert_no_network "partial-clone rejection is pre-network"

reset_case
git -C "$SUBJECT_ROOT" config remote.origin.promisor true
invoke_default
assert_status "promisor configuration is rejected" 1
assert_no_network "promisor rejection is pre-network"

reset_case
printf '%s\n' "$GATE_ROOT/.git/objects" >"$SUBJECT_ROOT/.git/objects/info/alternates"
invoke_default
assert_status "an on-disk object alternate is rejected" 1
assert_no_network "on-disk alternate rejection is pre-network"

reset_case
GIT_ALTERNATE_OBJECT_DIRECTORIES="$GATE_ROOT/.git/objects" invoke_default
assert_status "an environment-provided object alternate is rejected" 1
assert_no_network "environment alternate rejection is pre-network"

reset_case
SHARED_OBJECT_BACKUP="$WORK/subject-objects-$case_number"
cp -R "$SUBJECT_ROOT/.git/objects/." "$GATE_ROOT/.git/objects/"
mv "$SUBJECT_ROOT/.git/objects" "$SHARED_OBJECT_BACKUP"
ln -s "$GATE_ROOT/.git/objects" "$SUBJECT_ROOT/.git/objects"
invoke_default
rm "$SUBJECT_ROOT/.git/objects"
mv "$SHARED_OBJECT_BACKUP" "$SUBJECT_ROOT/.git/objects"
assert_status "repositories with the same canonical object store are rejected" 1
assert_no_network "shared object-store rejection is pre-network"

reset_case
write_run "$FAKE_BIN/run.1.body" queued null
write_run "$FAKE_BIN/run.2.body"
invoke_default
assert_silent_success "an incomplete run is polled to successful completion"
assert_eq "one incomplete poll sleeps exactly once" 10 "$(file_value_or_zero "$FAKE_BIN/sleeps")"
assert_eq "polling is immediate and ordered" \
  "curl $API_ROOT/$RUN_ID
sleep 10
curl $API_ROOT/$RUN_ID
curl $API_ROOT/$RUN_ID/attempts/$RUN_ATTEMPT/jobs?per_page=100&page=1
curl $API_ROOT/$RUN_ID/approvals
curl $API_ROOT/$RUN_ID" "$(if [[ -f "$FAKE_BIN/events" ]]; then cat "$FAKE_BIN/events"; fi)"

reset_case
write_run "$FAKE_BIN/run.1.body"
write_run "$FAKE_BIN/run.2.body" queued null "$RUN_ID" 3
invoke_default
assert_status "a rerun beginning before the final detail read is rejected" 1
assert_eq "rerun drift is detected by the final linearization read" 2 \
  "$(file_value_or_zero "$FAKE_BIN/run-count")"

reset_case
write_run "$FAKE_BIN/run.body" queued null
invoke_default
assert_status "thirty incomplete polls fail closed" 1
assert_eq "polling stops after exactly thirty reads" 30 "$(file_value_or_zero "$FAKE_BIN/run-count")"
assert_eq "there is no sleep after the final poll" 29 "$(line_count_or_zero "$FAKE_BIN/sleeps")"

for endpoint in run jobs approvals; do
  reset_case
  printf '500' >"$FAKE_BIN/$endpoint.status"
  invoke_default
  assert_status "$endpoint HTTP status failure is terminal" 1

  reset_case
  printf '2022-11-28' >"$FAKE_BIN/$endpoint.version"
  invoke_default
  assert_status "$endpoint API version mismatch is terminal" 1

  reset_case
  printf 'application/json; charset=us-ascii' >"$FAKE_BIN/$endpoint.media"
  invoke_default
  assert_status "$endpoint content type mismatch is terminal" 1

  reset_case
  : >"$FAKE_BIN/$endpoint.raw-metadata"
  invoke_default
  assert_status "$endpoint malformed metadata is terminal" 1

  reset_case
  : >"$FAKE_BIN/$endpoint.transport-failure"
  invoke_default
  assert_status "$endpoint curl failure is terminal" 1
done

for endpoint in run jobs approvals; do
  reset_case
  printf 'application/json' >"$FAKE_BIN/$endpoint.media"
  invoke_default
  assert_silent_success "$endpoint accepts bare application/json"
done

reset_case
invoke_default
expected_urls="$API_ROOT/$RUN_ID
$API_ROOT/$RUN_ID/attempts/$RUN_ATTEMPT/jobs?per_page=100&page=1
$API_ROOT/$RUN_ID/approvals
$API_ROOT/$RUN_ID"
actual_urls=$(sed -n 's/^curl //p' "$FAKE_BIN/events" 2>/dev/null || true)
assert_eq "only the four exact read endpoints and query are used" "$expected_urls" "$actual_urls"
assert_complete_curl_request "initial run GET has exact closed curl argv" 1 "$API_ROOT/$RUN_ID"
assert_complete_curl_request "jobs GET has exact closed curl argv" 2 \
  "$API_ROOT/$RUN_ID/attempts/$RUN_ATTEMPT/jobs?per_page=100&page=1"
assert_complete_curl_request "approvals GET has exact closed curl argv" 3 \
  "$API_ROOT/$RUN_ID/approvals"
assert_complete_curl_request "final run GET has exact closed curl argv" 4 "$API_ROOT/$RUN_ID"

for mutation in \
  '"9007199254740993" 2' '9007199254740993.0 2' '0 2' \
  '18446744073709551616 2' '9007199254740992 2' \
  '9007199254740993 "2"' '9007199254740993 2.0' \
  '9007199254740993 0' '9007199254740993 4294967296'; do
  read -r bad_id bad_attempt <<<"$mutation"
  reset_case
  write_run "$FAKE_BIN/run.body" completed '"success"' "$bad_id" "$bad_attempt"
  invoke_default
  assert_status "run identifiers reject $mutation without lossy conversion" 1
done

for field_value in \
  'event workflow_dispatch' \
  'path .github/workflows/publish-build-container.yml' \
  'path .github/workflows/publish-build-container.yml@build-container-v8' \
  "head_sha $(printf '6%.0s' {1..40})" \
  'head_branch build-container-v8'; do
  read -r field value <<<"$field_value"
  reset_case
  event=push
  workflow_path=.github/workflows/publish-build-container.yml@$TAG
  head_sha=$SOURCE
  head_branch=$TAG
  case "$field" in
    event) event=$value ;;
    path) workflow_path=$value ;;
    head_sha) head_sha=$value ;;
    head_branch) head_branch=$value ;;
  esac
  write_run "$FAKE_BIN/run.body" completed '"success"' "$RUN_ID" "$RUN_ATTEMPT" \
    "$event" "$workflow_path" "$head_sha" "$head_branch"
  invoke_default
  assert_status "run identity rejects $field substitution" 1
done

reset_case
write_run "$FAKE_BIN/run.1.body" queued null
write_run "$FAKE_BIN/run.2.body" completed '"success"' "$RUN_ID" "$RUN_ATTEMPT" \
  push ".github/workflows/publish-build-container.yml@$TAG" "$SOURCE" build-container-v8
invoke_default
assert_status "immutable run identity drift between polls is rejected" 1
assert_eq "identity drift is not retried" 2 "$(file_value_or_zero "$FAKE_BIN/run-count")"

reset_case
write_run "$FAKE_BIN/run.body" completed '"failure"'
invoke_default
assert_status "a completed unsuccessful run is rejected" 1

reset_case
printf '%s' '[]' >"$FAKE_BIN/run.body"
invoke_default
assert_status "a non-object run response is rejected" 1

reset_case
jq '.total_count=3' "$FAKE_BIN/jobs.body" >"$WORK/jobs" && mv "$WORK/jobs" "$FAKE_BIN/jobs.body"
invoke_default
assert_status "jobs total_count must be exactly two" 1

reset_case
sed 's/"total_count":2/"total_count":2.0/' "$FAKE_BIN/jobs.body" >"$WORK/jobs"
mv "$WORK/jobs" "$FAKE_BIN/jobs.body"
invoke_default
assert_status "jobs total_count must use JSON integer spelling" 1

reset_case
jq '.jobs |= .[0:1]' "$FAKE_BIN/jobs.body" >"$WORK/jobs" && mv "$WORK/jobs" "$FAKE_BIN/jobs.body"
invoke_default
assert_status "jobs array length must be exactly two" 1

reset_case
jq '.jobs[1].name="build-and-verify"' "$FAKE_BIN/jobs.body" >"$WORK/jobs" && mv "$WORK/jobs" "$FAKE_BIN/jobs.body"
invoke_default
assert_status "publisher job names must be unique and exact" 1

for mutation in \
  '.jobs[0].conclusion="failure"' '.jobs[0].status="in_progress"' \
  '.jobs[0].head_sha="0000000000000000000000000000000000000000"' \
  '.jobs[0].run_attempt=1' '.jobs[0].run_attempt="2"' \
  'del(.jobs[0].steps[1])' '.jobs[0].steps[1].conclusion="failure"' \
  '.jobs[0].steps += [{"name":"assert-exact-publisher-context","conclusion":"success"}]'; do
  reset_case
  jq "$mutation" "$FAKE_BIN/jobs.body" >"$WORK/jobs" && mv "$WORK/jobs" "$FAKE_BIN/jobs.body"
  invoke_default
  assert_status "jobs reject $mutation" 1
done

reset_case
sed 's/"run_attempt":2/"run_attempt":2.0/' "$FAKE_BIN/jobs.body" >"$WORK/jobs"
mv "$WORK/jobs" "$FAKE_BIN/jobs.body"
invoke_default
assert_status "job run_attempt must use JSON integer spelling when present" 1

reset_case
printf '<https://api.github.test/page=2>; rel="next"' >"$FAKE_BIN/jobs.link"
invoke_default
assert_status "a jobs Link continuation is rejected" 1

reset_case
printf '{}' >"$FAKE_BIN/approvals.body"
invoke_default
assert_status "the approvals endpoint must return exactly one array" 1

reset_case
write_approvals
invoke_default
assert_status "a missing current approval is rejected" 1

reset_case
current=$(review_json approved build-container-release "$(protocol_comment)")
write_approvals "$current" "$current"
invoke_default
assert_status "duplicate current approval records are rejected" 1

for state in rejected pending bypassed; do
  reset_case
  write_current_review "$state"
  invoke_default
  assert_status "a $state current review is rejected" 1
done

reset_case
write_current_review approved wrong-environment
invoke_default
assert_status "a current review for the wrong environment is rejected" 1

reset_case
write_current_review approved build-container-release "$(protocol_comment)" another-reviewer
invoke_default
assert_status "the API reviewer must equal the evidence approver" 1

reset_case
write_approvals "$(reviewer_only_json approved build-container-release "$(protocol_comment)")"
invoke_default
assert_status "a reviewer-only approval identity is rejected" 1

reset_case
write_current_review approved build-container-release \
  "$(protocol_comment "$RUN_ATTEMPT" "$RUN_ID" "$(printf '6%.0s' {1..64})")"
invoke_default
assert_status "a mismatched current protocol field is rejected" 1

reset_case
duplicate_comment="edgezero-release-evidence-v1 {\"challenge\":\"$CHALLENGE\",\"challenge\":\"$CHALLENGE\",\"image-digest\":\"$DIGEST\",\"png-sha256\":\"$SCREENSHOT\",\"release-tag\":\"$TAG\",\"reviewed-at\":\"$REVIEWED_AT\",\"run-attempt\":\"$RUN_ATTEMPT\",\"run-id\":\"$RUN_ID\",\"source-revision\":\"$SOURCE\"}"
write_current_review approved build-container-release "$duplicate_comment"
invoke_default
assert_status "raw duplicate protocol keys are rejected" 1

reset_case
malformed='edgezero-release-evidence-v1 {"challenge":'
write_current_review approved build-container-release "$malformed"
invoke_default
assert_status "malformed current protocol JSON is rejected" 1

reset_case
earlier=$(review_json rejected wrong-environment \
  "$(protocol_comment 1 "$RUN_ID" "$(printf '8%.0s' {1..64})")" nobody)
current=$(review_json approved build-container-release "$(protocol_comment)")
write_approvals "$earlier" "$current"
invoke_default
assert_silent_success "well-formed earlier attempts are inert"

reset_case
earlier_bad=$(review_json approved build-container-release \
  'edgezero-release-evidence-v1 {"run-attempt":"1"}')
current=$(review_json approved build-container-release "$(protocol_comment)")
write_approvals "$earlier_bad" "$current"
invoke_default
assert_status "malformed earlier protocol comments still fail" 1

reset_case
future=$(review_json approved build-container-release "$(protocol_comment 3)")
current=$(review_json approved build-container-release "$(protocol_comment)")
write_approvals "$current" "$future"
invoke_default
assert_status "future-attempt protocol comments are rejected" 1

reset_case
wrong_run=$(review_json approved build-container-release \
  "$(protocol_comment "$RUN_ATTEMPT" 9007199254740994)")
write_approvals "$wrong_run"
invoke_default
assert_status "a protocol comment for a different run is rejected" 1

reset_case
: >"$FAKE_BIN/run.transport-failure"
invoke_default
if ! rg -F -q -e "$TOKEN_VALUE" -e secret-response-body -e "$IMAGE_CONTENT" \
  -e "$EVIDENCE_CONTENT" "$WORK/stdout" "$WORK/stderr"; then
  ok "curl errors, response bodies, records, and tokens are redacted"
else
  no "curl errors, response bodies, records, and tokens are redacted"
fi

reset_case
invoke_default
if [[ -f "$FAKE_BIN/args-1" ]] &&
  ! rg -q '^POST$|^PATCH$|^PUT$|^DELETE$|--data|--upload-file' "$FAKE_BIN"/args-*; then
  ok "the verifier performs no mutation requests"
else
  no "the verifier performs no mutation requests"
fi

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
