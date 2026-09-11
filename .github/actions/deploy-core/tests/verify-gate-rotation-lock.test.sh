#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REAL_VERIFY="$DIR/../../../docker/build-app-cli/verify-gate-rotation-lock.sh"
REAL_JQ=$(command -v jq)
REAL_BASH=$(command -v bash)
WORK=$(mktemp -d)
WORK=$(cd -- "$WORK" && pwd -P)
trap 'rm -rf -- "$WORK"' EXIT

TOKEN_VALUE='fixture-github-token'
RUN_ID=9007199254740993
RUN_ATTEMPT=2
RUN_NUMBER=10
OLD_RUN_ID=18446744073709551614
OLD_RUN_ATTEMPT=1
OLD_RUN_NUMBER=9
POLICY_DIGEST="sha256:$(printf '7%.0s' {1..64})"
OUTER_DIGEST="sha256:$(printf '8%.0s' {1..64})"
PREVIOUS_DIGEST="sha256:$(printf '9%.0s' {1..64})"
SOURCE_PR=42
EVIDENCE_COMMENT_ID=9007199254740997
EVIDENCE_URL="https://github.com/stackpop/edgezero/pull/$SOURCE_PR#issuecomment-$EVIDENCE_COMMENT_ID"
ROTATION_PATH='.github/workflows/rotate-build-container-gate.yml@main'
API_BASE=https://api.github.com/repos/stackpop/edgezero
HISTORY_URL="$API_BASE/actions/workflows/rotate-build-container-gate.yml/runs?event=workflow_dispatch&per_page=100&page=1"

format_epoch() {
  local epoch=$1
  if date -u -r "$epoch" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null; then
    return
  fi
  date -u -d "@$epoch" '+%Y-%m-%dT%H:%M:%SZ'
}

NOW_EPOCH=$(date -u '+%s')
OLD_CREATED=$(format_epoch $((NOW_EPOCH - 720)))
RUN_CREATED=$(format_epoch $((NOW_EPOCH - 600)))
AUDITED_AT=$(format_epoch $((NOW_EPOCH - 300)))
REVIEWED_AT=$(format_epoch $((NOW_EPOCH - 240)))
STEP_COMPLETED=$(format_epoch $((NOW_EPOCH - 180)))
STALE_AUDITED=$(format_epoch $((NOW_EPOCH - 1200)))

pass=0
fail=0

ok() {
  printf '  \033[32mok\033[0m   %s\n' "$1"
  pass=$((pass + 1))
}

no() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1" >&2
  fail=$((fail + 1))
}

GATE_ROOT="$WORK/gate"
INPUT_ROOT="$WORK/input"
FAKE_BIN="$WORK/fake-bin"
mkdir -p "$GATE_ROOT/.github/docker/build-app-cli" "$INPUT_ROOT" "$FAKE_BIN"
cp "$REAL_VERIFY" "$GATE_ROOT/.github/docker/build-app-cli/verify-gate-rotation-lock.sh"
chmod 0755 "$GATE_ROOT/.github/docker/build-app-cli/verify-gate-rotation-lock.sh"
printf 'old-gate\n' >"$GATE_ROOT/.github/docker/build-app-cli/gate-marker"
printf '%s\n' \
  '.github/docker/build-app-cli/gate-marker' \
  '.github/docker/build-app-cli/gate-paths.txt' \
  '.github/docker/build-app-cli/verify-gate-rotation-lock.sh' \
  >"$GATE_ROOT/.github/docker/build-app-cli/gate-paths.txt"

git -C "$GATE_ROOT" init -q -b main
git -C "$GATE_ROOT" config user.name fixture
git -C "$GATE_ROOT" config user.email fixture@example.invalid
git -C "$GATE_ROOT" add .
git -C "$GATE_ROOT" commit -q -m old-gate
G=$(git -C "$GATE_ROOT" rev-parse HEAD)

printf 'dispatch\n' >"$GATE_ROOT/dispatch-marker"
git -C "$GATE_ROOT" add dispatch-marker
git -C "$GATE_ROOT" commit -q -m dispatch-snapshot
QD=$(git -C "$GATE_ROOT" rev-parse HEAD)

printf 'new-gate\n' >"$GATE_ROOT/.github/docker/build-app-cli/gate-marker"
git -C "$GATE_ROOT" add .github/docker/build-app-cli/gate-marker
git -C "$GATE_ROOT" commit -q -m new-gate
G_PRIME=$(git -C "$GATE_ROOT" rev-parse HEAD)

printf 'final\n' >"$GATE_ROOT/final-marker"
git -C "$GATE_ROOT" add final-marker
git -C "$GATE_ROOT" commit -q -m final-head
QF=$(git -C "$GATE_ROOT" rev-parse HEAD)

printf 'source\n' >"$GATE_ROOT/source-marker"
git -C "$GATE_ROOT" add source-marker
git -C "$GATE_ROOT" commit -q -m release-source
S=$(git -C "$GATE_ROOT" rev-parse HEAD)

git -C "$GATE_ROOT" checkout -q -b main-content-drift "$S"
printf 'drifted-gate\n' >"$GATE_ROOT/.github/docker/build-app-cli/gate-marker"
git -C "$GATE_ROOT" add .github/docker/build-app-cli/gate-marker
git -C "$GATE_ROOT" commit -q -m main-content-drift
MAIN_CONTENT_DRIFT=$(git -C "$GATE_ROOT" rev-parse HEAD)

git -C "$GATE_ROOT" checkout -q -b main-mode-drift "$S"
chmod 0755 "$GATE_ROOT/.github/docker/build-app-cli/gate-marker"
git -C "$GATE_ROOT" add .github/docker/build-app-cli/gate-marker
git -C "$GATE_ROOT" commit -q -m main-mode-drift
MAIN_MODE_DRIFT=$(git -C "$GATE_ROOT" rev-parse HEAD)

git -C "$GATE_ROOT" checkout -q -b main-type-drift "$S"
git -C "$GATE_ROOT" rm -q .github/docker/build-app-cli/gate-marker
mkdir "$GATE_ROOT/.github/docker/build-app-cli/gate-marker"
printf 'nested\n' >"$GATE_ROOT/.github/docker/build-app-cli/gate-marker/value"
git -C "$GATE_ROOT" add .github/docker/build-app-cli/gate-marker/value
git -C "$GATE_ROOT" commit -q -m main-type-drift
MAIN_TYPE_DRIFT=$(git -C "$GATE_ROOT" rev-parse HEAD)

git -C "$GATE_ROOT" checkout -q -b main-absence-drift "$S"
git -C "$GATE_ROOT" rm -q .github/docker/build-app-cli/gate-marker
git -C "$GATE_ROOT" commit -q -m main-absence-drift
MAIN_ABSENCE_DRIFT=$(git -C "$GATE_ROOT" rev-parse HEAD)

git -C "$GATE_ROOT" checkout -q -b rollback "$G_PRIME"
git -C "$GATE_ROOT" show "$G:.github/docker/build-app-cli/gate-marker" \
  >"$GATE_ROOT/.github/docker/build-app-cli/gate-marker"
git -C "$GATE_ROOT" add .github/docker/build-app-cli/gate-marker
git -C "$GATE_ROOT" commit -q -m restore-old-gate
printf 'rollback-final\n' >"$GATE_ROOT/rollback-final-marker"
git -C "$GATE_ROOT" add rollback-final-marker
git -C "$GATE_ROOT" commit -q -m rollback-final-head
QF_ROLLBACK=$(git -C "$GATE_ROOT" rev-parse HEAD)
git -C "$GATE_ROOT" branch source "$S"
git -C "$GATE_ROOT" checkout -q --detach "$G"

VERIFY="$GATE_ROOT/.github/docker/build-app-cli/verify-gate-rotation-lock.sh"
PREREQUISITE="$INPUT_ROOT/publisher-prerequisite.json"

cat >"$FAKE_BIN/jq" <<SH
#!/usr/bin/env bash
set -euo pipefail
fixture=\$(cd -- "\$(dirname -- "\$0")" && pwd)
[[ -z "\${GITHUB_TOKEN+x}\${GH_TOKEN+x}\${TOKEN+x}\${HOME+x}\${CURL_HOME+x}\${XDG_CONFIG_HOME+x}" ]]
[[ -z "\${HTTP_PROXY+x}\${HTTPS_PROXY+x}\${ALL_PROXY+x}\${NO_PROXY+x}\${AMBIENT_SECRET+x}" ]]
[[ -z "\${BASH_ENV+x}\${ENV+x}\${GIT_DIR+x}\${GIT_WORK_TREE+x}" ]]
count=0
[[ ! -f "\$fixture/jq-count" ]] || count=\$(<"\$fixture/jq-count")
printf '%s' \$((count + 1)) >"\$fixture/jq-count"
exec "$REAL_JQ" "\$@"
SH
chmod 0755 "$FAKE_BIN/jq"

cat >"$FAKE_BIN/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=$(cd -- "$(dirname -- "$0")" && pwd)
[[ "${LC_ALL:-}" == C ]]
[[ -z "${GITHUB_TOKEN+x}${GH_TOKEN+x}${TOKEN+x}${HOME+x}${CURL_HOME+x}${XDG_CONFIG_HOME+x}" ]]
[[ -z "${HTTP_PROXY+x}${HTTPS_PROXY+x}${ALL_PROXY+x}${NO_PROXY+x}${AMBIENT_SECRET+x}" ]]
[[ -z "${BASH_ENV+x}${ENV+x}${GIT_DIR+x}${GIT_WORK_TREE+x}" ]]
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
    https://*) url=$1; shift ;;
    *) shift ;;
  esac
done
[[ -n "$output" && -n "$url" ]]
case "$url" in
  */actions/workflows/rotate-build-container-gate.yml/runs\?event=workflow_dispatch\&per_page=100\&page=*)
    page=${url##*page=}; endpoint="history-$page" ;;
  */actions/runs/*/attempts/*/jobs\?per_page=100\&page=*)
    page=${url##*page=}; endpoint="jobs-$page" ;;
  */actions/runs/*/approvals) endpoint=approvals ;;
  */actions/runs/*) endpoint=run ;;
  */git/ref/heads/main) endpoint=main ;;
  *) printf 'unexpected URL: %s\n' "$url" >&2; exit 97 ;;
esac
endpoint_count=0
[[ ! -f "$fixture/$endpoint-count" ]] || endpoint_count=$(<"$fixture/$endpoint-count")
endpoint_count=$((endpoint_count + 1))
printf '%s' "$endpoint_count" >"$fixture/$endpoint-count"
printf 'curl %s\n' "$url" >>"$fixture/events"
if [[ -f "$fixture/$endpoint.transport-failure" ]]; then
  printf '%s\n' 'curl: fixture-github-token secret-response-body' >&2
  exit 28
fi
body="$fixture/$endpoint.body"
metadata_suffix=
[[ ! -f "$fixture/$endpoint.$endpoint_count.body" ]] || body="$fixture/$endpoint.$endpoint_count.body"
[[ ! -f "$fixture/$endpoint.$endpoint_count.status" ]] || metadata_suffix=.$endpoint_count
[[ -f "$body" ]] || { printf 'missing fixture body: %s\n' "$body" >&2; exit 98; }
cat "$body" >"$output"
if [[ -f "$fixture/$endpoint.raw-metadata" ]]; then
  cat "$fixture/$endpoint.raw-metadata"
else
  printf '%s\n%s\n%s\nlink=%s' \
    "$(<"$fixture/$endpoint$metadata_suffix.status")" \
    "$(<"$fixture/$endpoint$metadata_suffix.version")" \
    "$(<"$fixture/$endpoint$metadata_suffix.media")" \
    "$(<"$fixture/$endpoint$metadata_suffix.link")"
fi
SH
chmod 0755 "$FAKE_BIN/curl"

hash_bytes() {
  local value=$1 output
  output=$(printf '%s' "$value" | sha256sum)
  printf '%s' "${output%% *}"
}

write_run() {
  local status=${1:-completed} conclusion=${2:-'"success"'} id=${3:-$RUN_ID}
  local attempt=${4:-$RUN_ATTEMPT} number=${5:-$RUN_NUMBER} head=${6:-$QD}
  local actor=${7:-rotation-operator} event=${8:-workflow_dispatch}
  local path=${9:-$ROTATION_PATH} repository=${10:-stackpop/edgezero}
  local created=${11:-$RUN_CREATED} head_branch=${12:-main}
  printf '%s' "{\"id\":$id,\"run_number\":$number,\"run_attempt\":$attempt,\"created_at\":\"$created\",\"status\":\"$status\",\"conclusion\":$conclusion,\"event\":\"$event\",\"path\":\"$path\",\"head_sha\":\"$head\",\"head_branch\":\"$head_branch\",\"actor\":{\"login\":\"$actor\"},\"repository\":{\"full_name\":\"$repository\"},\"head_repository\":{\"full_name\":\"$repository\"}}" >"$FAKE_BIN/run.body"
}

history_entry() {
  printf '{"id":%s,"run_number":%s,"run_attempt":%s,"created_at":"%s"}' "$2" "$1" "$3" "$4"
}

write_default_history() {
  local old selected
  old=$(history_entry "$OLD_RUN_NUMBER" "$OLD_RUN_ID" "$OLD_RUN_ATTEMPT" "$OLD_CREATED")
  selected=$(history_entry "$RUN_NUMBER" "$RUN_ID" "$RUN_ATTEMPT" "$RUN_CREATED")
  printf '%s' "{\"total_count\":2,\"workflow_runs\":[$selected,$old]}" >"$FAKE_BIN/history-1.body"
  HISTORY_SNAPSHOT="[{\"run-attempt\":\"$OLD_RUN_ATTEMPT\",\"run-id\":\"$OLD_RUN_ID\",\"run-number\":\"$OLD_RUN_NUMBER\"},{\"run-attempt\":\"$RUN_ATTEMPT\",\"run-id\":\"$RUN_ID\",\"run-number\":\"$RUN_NUMBER\"}]"
  HISTORY_DIGEST="sha256:$(hash_bytes "$HISTORY_SNAPSHOT")"
}

write_jobs() {
  local step_conclusion=${1:-success} step_completed=${2:-$STEP_COMPLETED}
  local attempt=${3:-$RUN_ATTEMPT} head=${4:-$QD}
  printf '%s' "{\"total_count\":2,\"jobs\":[{\"id\":7001,\"name\":\"acquire-publication-lock\",\"status\":\"completed\",\"conclusion\":\"success\",\"head_sha\":\"$head\",\"run_attempt\":$attempt,\"steps\":[{\"name\":\"acquire\",\"conclusion\":\"success\",\"completed_at\":\"$step_completed\"}]},{\"id\":7002,\"name\":\"wait-for-rotation-approval\",\"status\":\"completed\",\"conclusion\":\"success\",\"head_sha\":\"$head\",\"run_attempt\":$attempt,\"steps\":[{\"name\":\"assert-exact-rotation-context\",\"status\":\"completed\",\"conclusion\":\"$step_conclusion\",\"completed_at\":\"$step_completed\"}]}]}" >"$FAKE_BIN/jobs-1.body"
}

write_main() {
  local sha=${1:-$S} ref=${2:-refs/heads/main} type=${3:-commit}
  printf '%s' "{\"ref\":\"$ref\",\"object\":{\"sha\":\"$sha\",\"type\":\"$type\"}}" >"$FAKE_BIN/main.body"
}

rotation_comment() {
  local result=${1:-activated} final_head=${2:-$QF} new_gate=${3:-$G_PRIME}
  local old_gate=${4:-$G} attempt=${5:-$RUN_ATTEMPT} id=${6:-$RUN_ID}
  local dispatch=${7:-$QD} audit=${8:-$AUDITED_AT} review=${9:-$REVIEWED_AT}
  local final_gate=$new_gate
  [[ "$result" == activated ]] || final_gate=$old_gate
  local policy="{\"audited-at\":\"$audit\",\"dispatch-sha\":\"$dispatch\",\"gate-sha\":\"$final_gate\",\"head-sha\":\"$final_head\",\"lock-run-attempt\":\"$attempt\",\"lock-run-id\":\"$id\",\"policy-sha256\":\"$POLICY_DIGEST\",\"release-state\":\"enabled\",\"required-workflow-sha\":\"$final_gate\"}"
  local evidence
  evidence="sha256:$(hash_bytes "$policy")"
  printf '%s\n%s' \
    "edgezero-gate-rotation-v1 {\"evidence-sha256\":\"$evidence\",\"head-sha\":\"$final_head\",\"lock-run-id\":\"$id\",\"new-gate-sha\":\"$new_gate\",\"old-gate-sha\":\"$old_gate\",\"result\":\"$result\",\"reviewed-at\":\"$review\"}" \
    "edgezero-gate-rotation-policy-v1 $policy"
}

review_json() {
  local state=$1 environment=$2 comment=$3 reviewer=${4:-rotation-reviewer}
  "$REAL_JQ" -cn --arg state "$state" --arg environment "$environment" \
    --arg comment "$comment" --arg reviewer "$reviewer" \
    '{state:$state,environments:[{name:$environment}],user:{login:$reviewer},comment:$comment}'
}

write_approval() {
  local comment=${1:-$(rotation_comment)} state=${2:-approved}
  local environment=${3:-build-container-gate-rotation-lock} reviewer=${4:-rotation-reviewer}
  printf '[%s]' "$(review_json "$state" "$environment" "$comment" "$reviewer")" >"$FAKE_BIN/approvals.body"
}

write_verified_prerequisite() {
  local gate=${1:-$G_PRIME} source=${2:-$S} history_digest=${3:-$HISTORY_DIGEST}
  local evidence
  evidence=${4:-$(rotation_comment | sed -n '1s/^edgezero-gate-rotation-v1 {"evidence-sha256":"\([^"]*\)".*/\1/p')}
  local attempt=${5:-$RUN_ATTEMPT} id=${6:-$RUN_ID} number=${7:-$RUN_NUMBER}
  local created=${8:-$RUN_CREATED}
  printf '%s' "{\"evidence-sha256\":\"$OUTER_DIGEST\",\"evidence-url\":\"$EVIDENCE_URL\",\"gate-sha\":\"$gate\",\"previous-value-sha256\":\"$PREVIOUS_DIGEST\",\"rotation-history\":{\"created-at\":\"$created\",\"evidence-sha256\":\"$evidence\",\"history-sha256\":\"$history_digest\",\"run-attempt\":\"$attempt\",\"run-id\":\"$id\",\"run-number\":\"$number\",\"state\":\"verified\"},\"schema-version\":2,\"source-pr\":\"$SOURCE_PR\",\"source-revision\":\"$source\"}" >"$PREREQUISITE"
}

write_bootstrap_prerequisite() {
  printf '%s' "{\"evidence-sha256\":\"$OUTER_DIGEST\",\"evidence-url\":\"$EVIDENCE_URL\",\"gate-sha\":\"$G_PRIME\",\"previous-value-sha256\":\"$PREVIOUS_DIGEST\",\"rotation-history\":{\"state\":\"bootstrap-no-rotation\"},\"schema-version\":2,\"source-pr\":\"$SOURCE_PR\",\"source-revision\":\"$S\"}" >"$PREREQUISITE"
}

write_inert_bootstrap_prerequisite() {
  printf '%s' "{\"evidence-sha256\":\"$OUTER_DIGEST\",\"evidence-url\":null,\"gate-sha\":\"$G_PRIME\",\"previous-value-sha256\":\"$PREVIOUS_DIGEST\",\"rotation-history\":{\"state\":\"bootstrap-no-rotation\"},\"schema-version\":2,\"source-pr\":null,\"source-revision\":null}" >"$PREREQUISITE"
}

set_metadata_defaults() {
  local endpoint=$1
  printf '200' >"$FAKE_BIN/$endpoint.status"
  printf '2026-03-10' >"$FAKE_BIN/$endpoint.version"
  printf 'application/json; charset=utf-8' >"$FAKE_BIN/$endpoint.media"
  : >"$FAKE_BIN/$endpoint.link"
}

reset_api() {
  rm -f "$FAKE_BIN"/{args-*,config-*,curl-count,jq-count,*-count,events} \
    "$FAKE_BIN"/*.transport-failure "$FAKE_BIN"/*.raw-metadata \
    "$FAKE_BIN"/*.body "$FAKE_BIN"/*.status "$FAKE_BIN"/*.version \
    "$FAKE_BIN"/*.media "$FAKE_BIN"/*.link
  local endpoint
  for endpoint in history-1 run jobs-1 approvals main; do set_metadata_defaults "$endpoint"; done
}

prepare_waiting() {
  local result=${1:-activated}
  git -C "$GATE_ROOT" checkout -q --detach "$G"
  reset_api
  write_run in_progress null
  if [[ "$result" == activated ]]; then
    write_approval "$(rotation_comment activated "$QF")"
    write_main "$QF"
  else
    write_approval "$(rotation_comment rolled-back "$QF_ROLLBACK")"
    write_main "$QF_ROLLBACK"
  fi
}

prepare_publisher() {
  git -C "$GATE_ROOT" checkout -q --detach "$G_PRIME"
  reset_api
  write_default_history
  write_run
  write_jobs
  write_approval
  write_main "$S"
  write_verified_prerequisite
}

prepare_publisher_rollback() {
  local comment evidence
  git -C "$GATE_ROOT" checkout -q --detach "$G"
  reset_api
  write_default_history
  write_run
  write_jobs
  comment=$(rotation_comment rolled-back "$QF_ROLLBACK" "$G_PRIME" "$G")
  write_approval "$comment"
  write_main "$QF_ROLLBACK"
  evidence=$(sed -n '1s/^edgezero-gate-rotation-v1 {"evidence-sha256":"\([^"]*\)".*/\1/p' <<<"$comment")
  write_verified_prerequisite "$G" "$QF_ROLLBACK" "$HISTORY_DIGEST" "$evidence"
}

waiting_args() {
  printf '%s\0' waiting --gate-root "$GATE_ROOT" --old-gate-sha "$G" \
    --dispatch-sha "$QD" --run-id "$RUN_ID" --run-attempt "$RUN_ATTEMPT" \
    --run-actor-login rotation-operator
}

publisher_args() {
  printf '%s\0' publisher --gate-root "$GATE_ROOT" --gate-sha "$G_PRIME" \
    --source-revision "$S" --publisher-prerequisite-json "$PREREQUISITE"
}

invoke() {
  local -a args=("$@")
  if env PATH="$FAKE_BIN:$PATH" GITHUB_TOKEN="$TOKEN_VALUE" \
    HOME="$WORK/hostile-home" CURL_HOME="$WORK/hostile-curl" \
    XDG_CONFIG_HOME="$WORK/hostile-xdg" HTTP_PROXY=http://proxy.invalid \
    HTTPS_PROXY=http://proxy.invalid ALL_PROXY=http://proxy.invalid NO_PROXY=invalid \
    GH_TOKEN=ambient-gh-token AMBIENT_SECRET=must-not-propagate \
    BASH_ENV= ENV= GIT_DIR="$WORK/hostile-git" GIT_WORK_TREE="$WORK/hostile-tree" \
    bash "$VERIFY" "${args[@]}" >"$WORK/stdout" 2>"$WORK/stderr"; then
    CAPTURE_STATUS=0
  else
    CAPTURE_STATUS=$?
  fi
}

invoke_waiting() {
  local -a args=()
  while IFS= read -r -d '' arg; do args+=("$arg"); done < <(waiting_args)
  invoke "${args[@]}"
}

invoke_publisher() {
  local -a args=()
  while IFS= read -r -d '' arg; do args+=("$arg"); done < <(publisher_args)
  invoke "${args[@]}"
}

invoke_publisher_at() {
  local gate=$1 source=$2
  invoke publisher --gate-root "$GATE_ROOT" --gate-sha "$gate" \
    --source-revision "$source" --publisher-prerequisite-json "$PREREQUISITE"
}

invoke_publisher_with_inherited_options() {
  local -a args=()
  while IFS= read -r -d '' arg; do args+=("$arg"); done < <(publisher_args)
  if env PATH="$FAKE_BIN:$PATH" GITHUB_TOKEN="$TOKEN_VALUE" \
    GH_TOKEN=ambient-gh-token TOKEN=ambient-exported-token AMBIENT_SECRET=must-not-propagate \
    "$REAL_BASH" -xa "$VERIFY" "${args[@]}" >"$WORK/stdout" 2>"$WORK/stderr"; then
    CAPTURE_STATUS=0
  else
    CAPTURE_STATUS=$?
  fi
}

assert_status() {
  local expected=$1 description=$2
  if [[ "$CAPTURE_STATUS" -eq "$expected" ]]; then
    ok "$description"
  else
    no "$description"
    printf '    expected status %s, got %s: ' "$expected" "$CAPTURE_STATUS" >&2
    sed -n '1p' "$WORK/stderr" >&2
  fi
}

assert_silent_success() {
  local description=$1
  if [[ "$CAPTURE_STATUS" -eq 0 && ! -s "$WORK/stdout" && ! -s "$WORK/stderr" ]]; then
    ok "$description"
  else
    no "$description"
    printf '    status %s: ' "$CAPTURE_STATUS" >&2
    sed -n '1p' "$WORK/stderr" >&2
  fi
}

assert_no_network() {
  local description=$1
  if [[ ! -e "$FAKE_BIN/curl-count" ]]; then ok "$description"; else no "$description"; fi
}

assert_eq() {
  local expected=$1 actual=$2 description=$3
  if [[ "$actual" == "$expected" ]]; then ok "$description"; else no "$description"; fi
}

assert_error_contains() {
  local expected=$1 description=$2
  if grep -Fq -e "$expected" "$WORK/stderr"; then ok "$description"; else no "$description"; fi
}

file_or_zero() {
  if [[ -f "$1" ]]; then cat "$1"; else printf 0; fi
}

mutate_json() {
  local filter=$1 file=$2 temporary="$WORK/mutated.json"
  "$REAL_JQ" -c "$filter" "$file" >"$temporary" && mv "$temporary" "$file"
}

echo '== gate rotation lock verifier =='

if env -i PATH="$WORK/no-tools" GITHUB_TOKEN="$TOKEN_VALUE" \
  "$REAL_BASH" "$VERIFY" invalid >"$WORK/stdout" 2>"$WORK/stderr"; then
  CAPTURE_STATUS=0
else
  CAPTURE_STATUS=$?
fi
assert_status 2 'usage is builtin-only when no external tools are available'
assert_error_contains 'usage: verify-gate-rotation-lock.sh waiting' \
  'builtin-only usage emits the documented interface'

NO_DIRNAME_BIN="$WORK/no-dirname-bin"
mkdir "$NO_DIRNAME_BIN"
for tool in env git jq curl awk sed sort mktemp cmp sha256sum date wc tail od grep paste tr rm cat; do
  ln -s "$(command -v "$tool")" "$NO_DIRNAME_BIN/$tool"
done
prepare_waiting
missing_tool_args=()
while IFS= read -r -d '' arg; do missing_tool_args+=("$arg"); done < <(waiting_args)
if env -i PATH="$NO_DIRNAME_BIN" GITHUB_TOKEN="$TOKEN_VALUE" \
  "$REAL_BASH" "$VERIFY" "${missing_tool_args[@]}" >"$WORK/stdout" 2>"$WORK/stderr"; then
  CAPTURE_STATUS=0
else
  CAPTURE_STATUS=$?
fi
assert_status 2 'a missing dirname is a tooling error'
assert_error_contains 'rotation verifier requires dirname' \
  'dirname is included in the closed tool preflight'

prepare_waiting
invoke_waiting
assert_silent_success 'waiting mode accepts a valid activation receipt'
assert_eq 3 "$(file_or_zero "$FAKE_BIN/curl-count")" 'waiting performs exactly run, approval, and main reads'

prepare_waiting rolled-back
invoke_waiting
assert_silent_success 'waiting mode accepts an exact rollback restoration'

prepare_publisher
invoke_publisher
assert_silent_success 'publisher mode accepts a stable verified rotation history'
assert_eq 7 "$(file_or_zero "$FAKE_BIN/curl-count")" 'publisher performs two history reads and repeats selected detail'

prepare_publisher_rollback
invoke_publisher_at "$G" "$QF_ROLLBACK"
assert_silent_success 'publisher mode accepts a valid rollback receipt'

prepare_publisher
invoke_publisher_with_inherited_options
assert_status 0 'publisher disables inherited xtrace and allexport'
if [[ ! -s "$WORK/stdout" ]] &&
  ! grep -Fq -e "$TOKEN_VALUE" -e ambient-exported-token "$WORK/stderr" &&
  [[ "$(wc -l <"$WORK/stderr" | tr -d '[:space:]')" -le 1 ]]; then
  ok 'inherited shell options do not expose tokens or propagate tracing to child tools'
else
  no 'inherited shell options do not expose tokens or propagate tracing to child tools'
fi

for mode in waiting publisher; do
  prepare_waiting
  if [[ "$mode" == publisher ]]; then prepare_publisher; fi

  invoke "$mode" --gate-root "$GATE_ROOT"
  assert_status 2 "$mode rejects missing flags as usage"
  assert_no_network "$mode missing flags fail before network"

  invoke "$mode" --unknown value
  assert_status 2 "$mode rejects unknown flags as usage"
  assert_no_network "$mode unknown flags fail before network"

  if [[ "$mode" == waiting ]]; then
    invoke waiting --gate-root "$GATE_ROOT" --gate-root "$GATE_ROOT" \
      --old-gate-sha "$G" --dispatch-sha "$QD" --run-id "$RUN_ID" \
      --run-attempt "$RUN_ATTEMPT" --run-actor-login rotation-operator
  else
    invoke publisher --gate-root "$GATE_ROOT" --gate-root "$GATE_ROOT" \
      --gate-sha "$G_PRIME" --source-revision "$S" \
      --publisher-prerequisite-json "$PREREQUISITE"
  fi
  assert_status 2 "$mode rejects duplicate flags as usage"
  assert_no_network "$mode duplicate flags fail before network"
done

prepare_waiting
invoke waiting --gate-root "$GATE_ROOT" --old-gate-sha "$G" --dispatch-sha "$QD" \
  --run-id "$RUN_ID" --run-attempt "$RUN_ATTEMPT" --run-actor-login rotation-operator \
  --final-head-sha "$QF"
assert_status 2 'caller-supplied derived waiting receipt fields are rejected'
assert_no_network 'derived waiting fields fail before network'

prepare_publisher
invoke publisher --gate-root "$GATE_ROOT" --gate-sha "$G_PRIME" \
  --source-revision "$S" --publisher-prerequisite-json "$PREREQUISITE" \
  --run-id "$RUN_ID"
assert_status 2 'caller-supplied derived publisher receipt fields are rejected'
assert_no_network 'derived publisher fields fail before network'

prepare_waiting
if env -u GITHUB_TOKEN PATH="$FAKE_BIN:$PATH" bash "$VERIFY" waiting \
  --gate-root "$GATE_ROOT" --old-gate-sha "$G" --dispatch-sha "$QD" \
  --run-id "$RUN_ID" --run-attempt "$RUN_ATTEMPT" --run-actor-login rotation-operator \
  >"$WORK/stdout" 2>"$WORK/stderr"; then CAPTURE_STATUS=0; else CAPTURE_STATUS=$?; fi
assert_status 1 'an absent GitHub token is rejected'
assert_no_network 'an absent token fails before network'

for field in id attempt number event path head actor repository branch status conclusion empty-conclusion; do
  prepare_waiting
  case "$field" in
    id) write_run in_progress null 9007199254740992 ;;
    attempt) write_run in_progress null "$RUN_ID" 1 ;;
    number) write_run in_progress null "$RUN_ID" "$RUN_ATTEMPT" 0 ;;
    event) write_run in_progress null "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QD" rotation-operator push ;;
    path) write_run in_progress null "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QD" rotation-operator workflow_dispatch '.github/workflows/other.yml@main' ;;
    head) write_run in_progress null "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QF" ;;
    actor) write_run in_progress null "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QD" another-actor ;;
    repository) write_run in_progress null "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QD" rotation-operator workflow_dispatch "$ROTATION_PATH" fork/edgezero ;;
    branch) write_run in_progress null "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QD" rotation-operator workflow_dispatch "$ROTATION_PATH" stackpop/edgezero "$RUN_CREATED" feature ;;
    status) write_run queued null ;;
    conclusion) write_run in_progress '"success"' ;;
    empty-conclusion) write_run in_progress '""' ;;
  esac
  invoke_waiting
  assert_status 1 "waiting rejects a mismatched run $field"
done

for number_case in \
  '"9007199254740993" 2 10' '9007199254740993.0 2 10' \
  '0 2 10' '18446744073709551616 2 10' \
  '9007199254740993 "2" 10' '9007199254740993 2.0 10' \
  '9007199254740993 0 10' '9007199254740993 4294967296 10' \
  '9007199254740993 2 "10"' '9007199254740993 2 10.0' \
  '9007199254740993 2 18446744073709551616'; do
  read -r id attempt number <<<"$number_case"
  prepare_waiting
  write_run in_progress null "$id" "$attempt" "$number"
  invoke_waiting
  assert_status 1 "waiting rejects noncanonical or out-of-range numbers: $number_case"
done

prepare_waiting
write_approval "$(rotation_comment)" approved build-container-gate-rotation-lock rotation-operator
invoke_waiting
assert_status 1 'waiting requires reviewer and run actor separation'

prepare_waiting
mutate_json '.[0].reviewer=.[0].user | del(.[0].user)' "$FAKE_BIN/approvals.body"
invoke_waiting
assert_status 1 'waiting rejects reviewer-only workflow review history'

for approval_case in duplicate rejected wrong-environment malformed digest head old-gate final-gate stale future-attempt; do
  prepare_waiting
  case "$approval_case" in
    duplicate)
      item=$(review_json approved build-container-gate-rotation-lock "$(rotation_comment)")
      printf '[%s,%s]' "$item" "$item" >"$FAKE_BIN/approvals.body" ;;
    rejected) write_approval "$(rotation_comment)" rejected ;;
    wrong-environment) write_approval "$(rotation_comment)" approved other ;;
    malformed) write_approval 'edgezero-gate-rotation-v1 {"broken":true}' ;;
    digest)
      comment=$(rotation_comment)
      comment=${comment/sha256:/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa}
      write_approval "$comment" ;;
    head) write_approval "$(rotation_comment activated "$S")" ;;
    old-gate) write_approval "$(rotation_comment activated "$QF" "$G_PRIME" "$G_PRIME")" ;;
    final-gate)
      comment=$(rotation_comment)
      comment=${comment/\"gate-sha\":\"$G_PRIME\"/\"gate-sha\":\"$G\"}
      write_approval "$comment" ;;
    stale) write_approval "$(rotation_comment activated "$QF" "$G_PRIME" "$G" "$RUN_ATTEMPT" "$RUN_ID" "$QD" "$STALE_AUDITED")" ;;
    future-attempt) write_approval "$(rotation_comment activated "$QF" "$G_PRIME" "$G" 3)" ;;
  esac
  invoke_waiting
  assert_status 1 "waiting rejects $approval_case approval evidence"
done

prepare_waiting
write_main "$S"
invoke_waiting
assert_status 1 'waiting requires both receipt heads to equal the current main ref'

prepare_waiting
write_main "$QF" refs/heads/other
invoke_waiting
assert_status 1 'waiting requires the exact main ref identity'

prepare_waiting
printf 'tampered\n' >"$GATE_ROOT/.github/docker/build-app-cli/gate-marker"
git -C "$GATE_ROOT" add .github/docker/build-app-cli/gate-marker
git -C "$GATE_ROOT" commit -q -m tampered-final
TAMPERED=$(git -C "$GATE_ROOT" rev-parse HEAD)
git -C "$GATE_ROOT" checkout -q --detach "$G"
write_approval "$(rotation_comment activated "$TAMPERED")"
write_main "$TAMPERED"
invoke_waiting
assert_status 1 'waiting rejects a final head with different manifested gate bytes'

for endpoint in run approvals main; do
  prepare_waiting
  : >"$FAKE_BIN/$endpoint.transport-failure"
  invoke_waiting
  assert_status 1 "waiting fails closed on $endpoint transport failure"
  if ! rg -q "$TOKEN_VALUE|secret-response-body" "$WORK/stderr"; then
    ok "waiting redacts $endpoint transport diagnostics"
  else
    no "waiting redacts $endpoint transport diagnostics"
  fi

  prepare_waiting
  printf '500' >"$FAKE_BIN/$endpoint.status"
  invoke_waiting
  assert_status 1 "waiting rejects $endpoint HTTP failure"

  prepare_waiting
  printf '2022-11-28' >"$FAKE_BIN/$endpoint.version"
  invoke_waiting
  assert_status 1 "waiting rejects $endpoint selected-version drift"

  prepare_waiting
  printf 'application/json; charset=iso-8859-1' >"$FAKE_BIN/$endpoint.media"
  invoke_waiting
  assert_status 1 "waiting rejects $endpoint media-type drift"
done

prepare_waiting
invoke_waiting
expected_waiting="$API_BASE/actions/runs/$RUN_ID
$API_BASE/actions/runs/$RUN_ID/approvals
$API_BASE/git/ref/heads/main"
actual_waiting=$(sed -n 's/^curl //p' "$FAKE_BIN/events")
assert_eq "$expected_waiting" "$actual_waiting" 'waiting uses only its exact three-route allowlist'

expected_transport='--disable
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
-'
expected_config="header = \"Accept: application/vnd.github+json\"
header = \"X-GitHub-Api-Version: 2026-03-10\"
header = \"User-Agent: edgezero-build-container-gate/1\"
header = \"Authorization: Bearer $TOKEN_VALUE\""
if [[ "$(sed -n '1,13p' "$FAKE_BIN/args-1")" == "$expected_transport" &&
  "$(<"$FAKE_BIN/config-1")" == "$expected_config" ]]; then
  ok 'every API request uses the exact transport and ordered header configuration'
else
  no 'every API request uses the exact transport and ordered header configuration'
fi

prepare_publisher
printf '\n' >>"$PREREQUISITE"
invoke_publisher
assert_status 1 'publisher rejects a prerequisite trailing newline'
assert_no_network 'noncanonical prerequisite bytes fail before network'

for mutation in \
  '."gate-sha"="0000000000000000000000000000000000000001"' \
  '."source-revision"="0000000000000000000000000000000000000001"' \
  '."schema-version"=1' \
  '.extra=true' \
  '."source-pr"="43"' \
  '."source-pr"="0"' \
  '."source-pr"="18446744073709551616"' \
  '."source-pr"=42' \
  '."evidence-url"="https://github.com/stackpop/edgezero/pull/43#issuecomment-9007199254740997"' \
  '."evidence-url"="https://github.com/stackpop/edgezero/pull/42#issuecomment-0"' \
  '."evidence-url"="https://github.com/stackpop/edgezero/pull/42#issuecomment-01"' \
  '."evidence-url"="https://github.com/stackpop/edgezero/pull/42#issuecomment-18446744073709551616"' \
  '."evidence-url"="https://github.com/stackpop/edgezero/pull/42#issuecomment-9007199254740997?view=1"' \
  '."rotation-history"."history-sha256"="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"' \
  '."rotation-history"."run-id"="9007199254740992"' \
  '."rotation-history"."run-attempt"="1"' \
  '."rotation-history"."run-number"="9"' \
  '."rotation-history"."created-at"="2020-01-01T00:00:00Z"'; do
  prepare_publisher
  mutate_json "$mutation" "$PREREQUISITE"
  invoke_publisher
  assert_status 1 "publisher rejects prerequisite mutation $mutation"
  assert_no_network "prerequisite mutation is rejected before network: $mutation"
done

for tuple_mutation in \
  '."source-revision"=null' \
  '."source-pr"=null' \
  '."evidence-url"=null'; do
  prepare_publisher
  mutate_json "$tuple_mutation" "$PREREQUISITE"
  invoke_publisher
  assert_status 1 "publisher rejects a partial release-bound tuple: $tuple_mutation"
  assert_no_network "partial release tuple fails before network: $tuple_mutation"
done

prepare_publisher
write_inert_bootstrap_prerequisite
invoke_publisher
assert_status 1 'publisher rejects an inert all-null source tuple'
assert_no_network 'inert publisher prerequisite fails before network'

prepare_publisher
printf '%s' "{\"evidence-sha256\":\"$OUTER_DIGEST\",\"gate-sha\":\"$G_PRIME\",\"previous-value-sha256\":\"$PREVIOUS_DIGEST\",\"rotation-history\":{\"state\":\"bootstrap-no-rotation\"},\"schema-version\":1,\"source-revision\":\"$S\"}" >"$PREREQUISITE"
invoke_publisher
assert_status 1 'publisher rejects the former schema-version-1 exact JCS shape'
assert_no_network 'schema-version-1 prerequisite fails before network'

prepare_publisher
sed 's/"source-pr":"42"/"source-pr":"42","source-pr":"42"/' "$PREREQUISITE" >"$WORK/duplicate-record"
mv "$WORK/duplicate-record" "$PREREQUISITE"
invoke_publisher
assert_status 1 'publisher rejects duplicate prerequisite JSON keys'
assert_no_network 'duplicate prerequisite keys fail before network'

prepare_publisher
printf '%s' '{"total_count":0,"workflow_runs":[]}' >"$FAKE_BIN/history-1.body"
write_bootstrap_prerequisite
invoke_publisher
assert_silent_success 'publisher accepts reviewed bootstrap history only when both snapshots are empty'
assert_eq 3 "$(file_or_zero "$FAKE_BIN/curl-count")" \
  'bootstrap performs two stable history enumerations and an exact main read'

prepare_publisher
printf '%s' '{"total_count":0,"workflow_runs":[]}' >"$FAKE_BIN/history-1.body"
write_bootstrap_prerequisite
write_main "$MAIN_CONTENT_DRIFT"
invoke_publisher
assert_status 1 'bootstrap rejects active-gate content drift on protected main'

prepare_publisher
write_bootstrap_prerequisite
invoke_publisher
assert_status 1 'bootstrap history is rejected once any rotation exists'

prepare_publisher
printf '%s' '{"total_count":0,"workflow_runs":[]}' >"$FAKE_BIN/history-1.body"
invoke_publisher
assert_status 1 'verified history is rejected when current history is empty'

for history_case in duplicate-id duplicate-number id-string id-fraction id-zero id-overflow \
  attempt-string attempt-fraction attempt-zero attempt-overflow number-string number-fraction \
  number-zero number-overflow future-created malformed-created count-string count-mismatch; do
  prepare_publisher
  old=$(history_entry "$OLD_RUN_NUMBER" "$OLD_RUN_ID" "$OLD_RUN_ATTEMPT" "$OLD_CREATED")
  selected=$(history_entry "$RUN_NUMBER" "$RUN_ID" "$RUN_ATTEMPT" "$RUN_CREATED")
  case "$history_case" in
    duplicate-id) selected=$(history_entry "$RUN_NUMBER" "$OLD_RUN_ID" "$RUN_ATTEMPT" "$RUN_CREATED") ;;
    duplicate-number) selected=$(history_entry "$OLD_RUN_NUMBER" "$RUN_ID" "$RUN_ATTEMPT" "$RUN_CREATED") ;;
    id-string) selected=${selected/\"id\":$RUN_ID/\"id\":\"$RUN_ID\"} ;;
    id-fraction) selected=${selected/\"id\":$RUN_ID/\"id\":$RUN_ID.0} ;;
    id-zero) selected=${selected/\"id\":$RUN_ID/\"id\":0} ;;
    id-overflow) selected=${selected/\"id\":$RUN_ID/\"id\":18446744073709551616} ;;
    attempt-string) selected=${selected/\"run_attempt\":$RUN_ATTEMPT/\"run_attempt\":\"$RUN_ATTEMPT\"} ;;
    attempt-fraction) selected=${selected/\"run_attempt\":$RUN_ATTEMPT/\"run_attempt\":2.0} ;;
    attempt-zero) selected=${selected/\"run_attempt\":$RUN_ATTEMPT/\"run_attempt\":0} ;;
    attempt-overflow) selected=${selected/\"run_attempt\":$RUN_ATTEMPT/\"run_attempt\":4294967296} ;;
    number-string) selected=${selected/\"run_number\":$RUN_NUMBER/\"run_number\":\"$RUN_NUMBER\"} ;;
    number-fraction) selected=${selected/\"run_number\":$RUN_NUMBER/\"run_number\":10.0} ;;
    number-zero) selected=${selected/\"run_number\":$RUN_NUMBER/\"run_number\":0} ;;
    number-overflow) selected=${selected/\"run_number\":$RUN_NUMBER/\"run_number\":18446744073709551616} ;;
    future-created) selected=${selected/$RUN_CREATED/$(format_epoch $((NOW_EPOCH + 60)))} ;;
    malformed-created) selected=${selected/$RUN_CREATED/2026-02-30T00:00:00Z} ;;
    count-string) printf '%s' "{\"total_count\":\"2\",\"workflow_runs\":[$selected,$old]}" >"$FAKE_BIN/history-1.body" ;;
    count-mismatch) printf '%s' "{\"total_count\":3,\"workflow_runs\":[$selected,$old]}" >"$FAKE_BIN/history-1.body" ;;
  esac
  if [[ "$history_case" != count-string && "$history_case" != count-mismatch ]]; then
    printf '%s' "{\"total_count\":2,\"workflow_runs\":[$selected,$old]}" >"$FAKE_BIN/history-1.body"
  fi
  invoke_publisher
  assert_status 1 "publisher rejects malformed history: $history_case"
done

prepare_publisher
sed 's/"id":9007199254740993/"id":9007199254740993,"id":9007199254740993/' \
  "$FAKE_BIN/history-1.body" >"$WORK/duplicate-number-key"
mv "$WORK/duplicate-number-key" "$FAKE_BIN/history-1.body"
invoke_publisher
assert_status 1 'publisher rejects duplicate raw numeric fields in history JSON'

prepare_publisher
sed 's/"run_attempt":1/"run_attempt":2/' \
  "$FAKE_BIN/history-1.body" >"$WORK/mutated.json"
mv "$WORK/mutated.json" "$FAKE_BIN/history-1.body"
invoke_publisher
assert_status 1 'an old-run rerun changes the complete history digest and blocks'

prepare_publisher
printf '{"total_count":1,"workflow_runs":[%s]}' \
  "$(history_entry "$RUN_NUMBER" "$RUN_ID" "$RUN_ATTEMPT" "$RUN_CREATED")" \
  >"$FAKE_BIN/history-1.body"
invoke_publisher
assert_status 1 'deletion or retention pruning of recorded history blocks'

prepare_publisher
later=$(history_entry 11 8 1 "$RUN_CREATED")
current=$(<"$FAKE_BIN/history-1.body")
current=${current/\"total_count\":2/\"total_count\":3}
current=${current/\"workflow_runs\":[/\"workflow_runs\":[$later,}
printf '%s' "$current" >"$FAKE_BIN/history-1.body"
invoke_publisher
assert_status 1 'a later-numbered rotation supersedes an older successful record even with a smaller id'

prepare_publisher
cp "$FAKE_BIN/history-1.body" "$FAKE_BIN/history-1.1.body"
sed 's/"run_attempt":1/"run_attempt":2/' \
  "$FAKE_BIN/history-1.body" >"$WORK/mutated.json"
mv "$WORK/mutated.json" "$FAKE_BIN/history-1.body"
invoke_publisher
assert_status 1 'history changes between complete enumerations fail closed'

prepare_publisher
cp "$FAKE_BIN/run.body" "$FAKE_BIN/run.1.body"
sed 's/"status":"completed","conclusion":"success"/"status":"in_progress","conclusion":null/' \
  "$FAKE_BIN/run.body" >"$WORK/mutated.json"
mv "$WORK/mutated.json" "$FAKE_BIN/run.body"
invoke_publisher
assert_status 1 'selected-run detail drift after the second snapshot fails closed'

for detail_case in list-id list-attempt list-number list-created status conclusion event path head repository branch; do
  prepare_publisher
  case "$detail_case" in
    list-id) write_run completed '"success"' 9007199254740992 ;;
    list-attempt) write_run completed '"success"' "$RUN_ID" 1 ;;
    list-number) write_run completed '"success"' "$RUN_ID" "$RUN_ATTEMPT" 9 ;;
    list-created) write_run completed '"success"' "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QD" rotation-operator workflow_dispatch "$ROTATION_PATH" stackpop/edgezero "$OLD_CREATED" ;;
    status) write_run in_progress null ;;
    conclusion) write_run completed '"failure"' ;;
    event) write_run completed '"success"' "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QD" rotation-operator push ;;
    path) write_run completed '"success"' "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QD" rotation-operator workflow_dispatch '.github/workflows/other.yml@main' ;;
    head) write_run completed '"success"' "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$S" ;;
    repository) write_run completed '"success"' "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QD" rotation-operator workflow_dispatch "$ROTATION_PATH" fork/edgezero ;;
    branch) write_run completed '"success"' "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER" "$QD" rotation-operator workflow_dispatch "$ROTATION_PATH" stackpop/edgezero "$RUN_CREATED" feature ;;
  esac
  invoke_publisher
  assert_status 1 "publisher rejects selected-run detail mismatch: $detail_case"
done

for jobs_case in total duplicate-id attempt head missing-step duplicate-step failed-step incomplete-step stale-completion link; do
  prepare_publisher
  case "$jobs_case" in
    total) mutate_json '.total_count=3' "$FAKE_BIN/jobs-1.body" ;;
    duplicate-id) mutate_json '.jobs[1].id=.jobs[0].id' "$FAKE_BIN/jobs-1.body" ;;
    attempt) mutate_json '.jobs[1].run_attempt=1' "$FAKE_BIN/jobs-1.body" ;;
    head) mutate_json '.jobs[1].head_sha="0000000000000000000000000000000000000001"' "$FAKE_BIN/jobs-1.body" ;;
    missing-step) mutate_json '.jobs[1].steps=[]' "$FAKE_BIN/jobs-1.body" ;;
    duplicate-step) mutate_json '.jobs[0].steps += [{"name":"assert-exact-rotation-context","status":"completed","conclusion":"success","completed_at":"'"$STEP_COMPLETED"'"}]' "$FAKE_BIN/jobs-1.body" ;;
    failed-step) write_jobs failure ;;
    incomplete-step) mutate_json '.jobs[1].steps[0].status="in_progress"' "$FAKE_BIN/jobs-1.body" ;;
    stale-completion) write_jobs success "$AUDITED_AT" ;;
    link) printf '<%s>; rel="next"' "$API_BASE/actions/runs/$RUN_ID/attempts/$RUN_ATTEMPT/jobs?per_page=100&page=2" >"$FAKE_BIN/jobs-1.link" ;;
  esac
  invoke_publisher
  assert_status 1 "publisher rejects exact-attempt jobs mismatch: $jobs_case"
done

prepare_publisher
write_run completed '"failure"'
write_verified_prerequisite
invoke_publisher
assert_status 1 'a failed rerun blocks despite a previously successful run identity'

prepare_publisher
write_main "$QF"
invoke_publisher
assert_status 1 'publisher requires source revision to remain on protected main'

for main_drift in \
  "$MAIN_CONTENT_DRIFT:content" \
  "$MAIN_MODE_DRIFT:mode" \
  "$MAIN_TYPE_DRIFT:type" \
  "$MAIN_ABSENCE_DRIFT:absence"; do
  prepare_publisher
  write_main "${main_drift%%:*}"
  invoke_publisher
  assert_status 1 "publisher rejects active-gate ${main_drift#*:} drift on protected main"
done

prepare_publisher
write_main "$S" refs/heads/other
invoke_publisher
assert_status 1 'publisher binds the exact protected main ref response'

prepare_publisher
item=$(review_json approved build-container-gate-rotation-lock "$(rotation_comment)")
printf '[%s,%s]' "$item" "$item" >"$FAKE_BIN/approvals.body"
invoke_publisher
assert_status 1 'publisher rejects duplicate authenticated rotation receipts'

prepare_publisher
write_approval "$(rotation_comment)" approved build-container-gate-rotation-lock rotation-operator
invoke_publisher
assert_status 1 'publisher enforces reviewer and run actor separation'

prepare_publisher
cp "$FAKE_BIN/history-1.body" "$FAKE_BIN/history-1.1.body"
cp "$FAKE_BIN/history-1.body" "$FAKE_BIN/history-1.2.body"
cp "$FAKE_BIN/run.body" "$FAKE_BIN/run.1.body"
cp "$FAKE_BIN/run.body" "$FAKE_BIN/run.2.body"
later=$(history_entry 11 8 1 "$RUN_CREATED")
printf '%s' "{\"total_count\":3,\"workflow_runs\":[$later]}" >"$FAKE_BIN/history-1.3.body"
invoke_publisher
assert_silent_success 'work appearing only after the second snapshot is later FIFO work'
assert_eq 2 "$(file_or_zero "$FAKE_BIN/history-1-count")" 'publisher takes exactly two history snapshots at its linearization point'

prepare_publisher
entries="$WORK/history.tsv"
: >"$entries"
page1='{"total_count":101,"workflow_runs":['
separator=
for ((number = 100; number >= 1; number--)); do
  id=$((1000 + number))
  page1+="$separator$(history_entry "$number" "$id" 1 "$OLD_CREATED")"
  separator=,
  printf '%s\t%s\t%s\t%s\n' "$number" "$id" 1 "$OLD_CREATED" >>"$entries"
done
page1+=']}'
printf '%s' "$page1" >"$FAKE_BIN/history-1.body"
printf '<%s>; rel="next"' \
  "$API_BASE/actions/workflows/rotate-build-container-gate.yml/runs?event=workflow_dispatch&per_page=100&page=2" \
  >"$FAKE_BIN/history-1.link"
set_metadata_defaults history-2
selected=$(history_entry 101 "$RUN_ID" "$RUN_ATTEMPT" "$RUN_CREATED")
printf '%s' "{\"total_count\":101,\"workflow_runs\":[$selected]}" >"$FAKE_BIN/history-2.body"
printf '%s\t%s\t%s\t%s\n' 101 "$RUN_ID" "$RUN_ATTEMPT" "$RUN_CREATED" >>"$entries"
snapshot='['
separator=
while IFS=$'\t' read -r number id attempt _; do
  snapshot+="$separator{\"run-attempt\":\"$attempt\",\"run-id\":\"$id\",\"run-number\":\"$number\"}"
  separator=,
done < <(sort -t $'\t' -k1,1n "$entries")
snapshot+=']'
HISTORY_DIGEST="sha256:$(hash_bytes "$snapshot")"
RUN_NUMBER=101
write_run completed '"success"' "$RUN_ID" "$RUN_ATTEMPT" "$RUN_NUMBER"
write_verified_prerequisite "$G_PRIME" "$S" "$HISTORY_DIGEST" \
  "$(rotation_comment | sed -n '1s/^edgezero-gate-rotation-v1 {"evidence-sha256":"\([^"]*\)".*/\1/p')" \
  "$RUN_ATTEMPT" "$RUN_ID" "$RUN_NUMBER"
invoke_publisher
assert_silent_success 'publisher consumes a complete 101-run paginated history twice'
assert_eq 2 "$(file_or_zero "$FAKE_BIN/history-2-count")" 'both history passes reach the synthesized second page'
RUN_NUMBER=10

for pagination_case in missing-next wrong-query wrong-page extra-relation repeated-page inconsistent-count skipped-page; do
  prepare_publisher
  selected=$(history_entry "$RUN_NUMBER" "$RUN_ID" "$RUN_ATTEMPT" "$RUN_CREATED")
  printf '%s' "{\"total_count\":101,\"workflow_runs\":[" >"$FAKE_BIN/history-1.body"
  separator=
  for ((i = 100; i >= 1; i--)); do
    printf '%s%s' "$separator" "$(history_entry "$i" "$((1000 + i))" 1 "$OLD_CREATED")" \
      >>"$FAKE_BIN/history-1.body"
    separator=,
  done
  printf ']}' >>"$FAKE_BIN/history-1.body"
  set_metadata_defaults history-2
  printf '%s' "{\"total_count\":101,\"workflow_runs\":[$selected]}" >"$FAKE_BIN/history-2.body"
  next="$API_BASE/actions/workflows/rotate-build-container-gate.yml/runs?event=workflow_dispatch&per_page=100&page=2"
  printf '<%s>; rel="next"' "$next" >"$FAKE_BIN/history-1.link"
  case "$pagination_case" in
    missing-next) : >"$FAKE_BIN/history-1.link" ;;
    wrong-query) printf '<%s>; rel="next"' "${next/event=workflow_dispatch/per_page=100}" >"$FAKE_BIN/history-1.link" ;;
    wrong-page) printf '<%s>; rel="next"' "${next/page=2/page=3}" >"$FAKE_BIN/history-1.link" ;;
    extra-relation) printf '<%s>; rel="next", <%s>; rel="next"' "$next" "$next" >"$FAKE_BIN/history-1.link" ;;
    repeated-page) cp "$FAKE_BIN/history-1.body" "$FAKE_BIN/history-2.body" ;;
    inconsistent-count) mutate_json '.total_count=102' "$FAKE_BIN/history-2.body" ;;
    skipped-page)
      printf '<%s>; rel="next"' "${next/page=2/page=100}" >"$FAKE_BIN/history-1.link" ;;
  esac
  invoke_publisher
  assert_status 1 "publisher rejects malformed or incomplete pagination: $pagination_case"
done

if grep -Fqx 'readonly MAX_PAGES=100' "$REAL_VERIFY"; then
  ok 'publisher production page bound remains exactly 100'
else
  no 'publisher production page bound remains exactly 100'
fi

# Reduce the committed fixture bound so both terminal-page branches run without 100 API pages.
git -C "$GATE_ROOT" checkout -q --detach "$G_PRIME"
sed 's/^readonly MAX_PAGES=100$/readonly MAX_PAGES=1/' \
  "$VERIFY" >"$WORK/limited-helper"
mv "$WORK/limited-helper" "$VERIFY"
chmod 0755 "$VERIFY"
git -C "$GATE_ROOT" add .github/docker/build-app-cli/verify-gate-rotation-lock.sh
git -C "$GATE_ROOT" commit -q -m reduced-page-bound-fixture
LIMIT_GATE=$(git -C "$GATE_ROOT" rev-parse HEAD)
printf 'limit-source\n' >"$GATE_ROOT/limit-source-marker"
git -C "$GATE_ROOT" add limit-source-marker
git -C "$GATE_ROOT" commit -q -m reduced-page-bound-source
LIMIT_SOURCE=$(git -C "$GATE_ROOT" rev-parse HEAD)
git -C "$GATE_ROOT" checkout -q --detach "$LIMIT_GATE"

full_page=
separator=
for ((i = 0; i < 100; i++)); do
  full_page+="${separator}null"
  separator=,
done

reset_api
write_default_history
printf '%s' "{\"total_count\":100,\"workflow_runs\":[$full_page]}" \
  >"$FAKE_BIN/history-1.body"
write_verified_prerequisite "$LIMIT_GATE" "$LIMIT_SOURCE"
invoke_publisher_at "$LIMIT_GATE" "$LIMIT_SOURCE"
assert_status 1 'history rejects a full terminal page at the page-100 bound'
assert_error_contains 'rotation history page bound ended on a full page' \
  'history terminal-page rejection occurs before entry parsing'

reset_api
write_default_history
write_run
printf '%s' "{\"total_count\":100,\"jobs\":[$full_page]}" >"$FAKE_BIN/jobs-1.body"
write_verified_prerequisite "$LIMIT_GATE" "$LIMIT_SOURCE"
invoke_publisher_at "$LIMIT_GATE" "$LIMIT_SOURCE"
assert_status 1 'jobs reject a full terminal page at the page-100 bound'
assert_error_contains 'rotation jobs page bound ended on a full page' \
  'jobs terminal-page rejection occurs before entry parsing'

git -C "$GATE_ROOT" checkout -q --detach "$G_PRIME"

for endpoint in history-1 run jobs-1 approvals main; do
  prepare_publisher
  : >"$FAKE_BIN/$endpoint.transport-failure"
  invoke_publisher
  assert_status 1 "publisher fails closed on $endpoint transport failure"
done

prepare_publisher
invoke_publisher
publisher_urls=$(sed -n 's/^curl //p' "$FAKE_BIN/events")
expected_publisher="$HISTORY_URL
$API_BASE/actions/runs/$RUN_ID
$API_BASE/actions/runs/$RUN_ID/attempts/$RUN_ATTEMPT/jobs?per_page=100&page=1
$API_BASE/actions/runs/$RUN_ID/approvals
$API_BASE/git/ref/heads/main
$HISTORY_URL
$API_BASE/actions/runs/$RUN_ID"
assert_eq "$expected_publisher" "$publisher_urls" 'publisher uses the exact ordered read-only route allowlist'
if rg -q '^GET$' "$FAKE_BIN"/args-* && ! rg -q '^POST$|^PATCH$|^PUT$|^DELETE$' "$FAKE_BIN"/args-*; then
  ok 'both modes issue explicit GET requests only'
else
  no 'both modes issue explicit GET requests only'
fi

prepare_publisher
printf '#!/usr/bin/env bash\nexit 1\n' >"$FAKE_BIN/jq"
chmod 0755 "$FAKE_BIN/jq"
invoke_publisher
assert_status 2 'an unusable required jq is a tooling error'
assert_no_network 'tooling failure occurs before network'

printf '\nPassed: %d  Failed: %d\n' "$pass" "$fail"
((fail == 0))
