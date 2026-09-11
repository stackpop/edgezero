#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REAL_ASSERT="$DIR/../../../docker/build-app-cli/assert-build-container-dispatch-context.sh"
WORK=$(mktemp -d)
WORK=$(cd -- "$WORK" && pwd -P)
trap 'rm -rf "$WORK"' EXIT

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

assert_pass() {
  local description=$1 status=0
  shift
  CASE_ROOT="$WORK/case-$((case_number += 1))"
  mkdir -p "$CASE_ROOT"
  if (($#)); then
    "$@" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$?
  else
    run_assert >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$?
  fi
  if [[ "$status" -eq 0 && ! -s "$CASE_ROOT/stdout" && ! -s "$CASE_ROOT/stderr" ]]; then
    ok "$description"
  else
    cat "$CASE_ROOT/stdout" "$CASE_ROOT/stderr" >&2
    no "$description"
  fi
}

assert_fail() {
  local description=$1
  CASE_ROOT="$WORK/case-$((case_number += 1))"
  mkdir -p "$CASE_ROOT"
  if run_assert >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr"; then
    no "$description"
  elif [[ -s "$CASE_ROOT/stdout" ]]; then
    cat "$CASE_ROOT/stdout" >&2
    no "$description emitted stdout"
  else
    ok "$description"
  fi
}

assert_fail_message() {
  local description=$1 expected=$2
  CASE_ROOT="$WORK/case-$((case_number += 1))"
  mkdir -p "$CASE_ROOT"
  if run_assert >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr"; then
    no "$description"
  elif [[ -s "$CASE_ROOT/stdout" ]] || ! grep -Fq -e "$expected" "$CASE_ROOT/stderr"; then
    cat "$CASE_ROOT/stdout" "$CASE_ROOT/stderr" >&2
    no "$description"
  else
    ok "$description"
  fi
}

REPO="$WORK/gate"
FAKE_BIN="$WORK/bin"
mkdir -p "$REPO/.github/docker/build-app-cli" "$REPO/.github" "$FAKE_BIN"
cp "$REAL_ASSERT" "$REPO/.github/docker/build-app-cli/assert-build-container-dispatch-context.sh"
chmod 0755 "$REPO/.github/docker/build-app-cli/assert-build-container-dispatch-context.sh"
printf 'FROM scratch\n' >"$REPO/.github/docker/build-app-cli/Dockerfile"
printf 'manifested\n' >"$REPO/.github/docker/build-app-cli/helper.sh"
printf '%s\n' \
  '.github/docker/build-app-cli/Dockerfile' \
  '.github/docker/build-app-cli/image-context-paths.txt' \
  >"$REPO/.github/docker/build-app-cli/image-context-paths.txt"
printf '%s\n' \
  '.github/CODEOWNERS' \
  '.github/docker/build-app-cli/Dockerfile' \
  '.github/docker/build-app-cli/assert-build-container-dispatch-context.sh' \
  '.github/docker/build-app-cli/gate-paths.txt' \
  '.github/docker/build-app-cli/helper.sh' \
  '.github/docker/build-app-cli/image-context-paths.txt' \
  >"$REPO/.github/docker/build-app-cli/gate-paths.txt"
while IFS= read -r path; do
  printf '/%s @stackpop/edgezero-build-container-gate-reviewers\n' "$path"
done <"$REPO/.github/docker/build-app-cli/gate-paths.txt" >"$REPO/.github/CODEOWNERS"
git -C "$REPO" init -q -b main
git -C "$REPO" config user.name fixture
git -C "$REPO" config user.email fixture@example.invalid
git -C "$REPO" add .
git -C "$REPO" commit -q -m gate
G=$(git -C "$REPO" rev-parse HEAD)
printf 'later non-gate state\n' >"$REPO/app.txt"
git -C "$REPO" add app.txt
git -C "$REPO" commit -q -m snapshot
Q=$(git -C "$REPO" rev-parse HEAD)
git -C "$REPO" checkout -q --detach "$G"
ASSERT="$REPO/.github/docker/build-app-cli/assert-build-container-dispatch-context.sh"

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

write_reply() {
  local head_repo=${1:-stackpop/edgezero} head_sha=${2:-$Q}
  local status=${3:-200} version=${4:-2026-03-10} media=${5:-application/json}
  jq -cn \
    --arg head_repo "$head_repo" \
    --arg head_sha "$head_sha" \
    '{number:17,state:"open",merged:false,base:{ref:"main",repo:{full_name:"stackpop/edgezero"}},head:{sha:$head_sha,repo:{full_name:$head_repo}}}' \
    >"$FAKE_BIN/body"
  {
    cat "$FAKE_BIN/body"
    printf '\n%s\n%s\n%s' "$status" "$version" "$media"
  } >"$FAKE_BIN/reply"
}

clear_overrides() {
  unset EVENT_NAME REPOSITORY REF REF_PROTECTED SHA WORKFLOW_SHA WORKFLOW_REF GATE_SHA RELEASE_STATE
  unset PR_NUMBER HEAD_REPOSITORY HEAD_SHA TOKEN GATE_ROOT
}

run_assert() {
  PATH="$FAKE_BIN:$PATH" \
    GITHUB_TOKEN="${TOKEN:-fixture-token}" \
    EDGEZERO_EVENT_NAME="${EVENT_NAME:-workflow_dispatch}" \
    EDGEZERO_REPOSITORY="${REPOSITORY:-stackpop/edgezero}" \
    EDGEZERO_REF="${REF:-refs/heads/main}" \
    EDGEZERO_REF_PROTECTED="${REF_PROTECTED:-true}" \
    EDGEZERO_SHA="${SHA:-$Q}" \
    EDGEZERO_WORKFLOW_SHA="${WORKFLOW_SHA:-$Q}" \
    EDGEZERO_WORKFLOW_REF="${WORKFLOW_REF:-stackpop/edgezero/.github/workflows/build-container-ci.yml@refs/heads/main}" \
    EDGEZERO_GATE_SHA="${GATE_SHA:-$G}" \
    EDGEZERO_RELEASE_STATE="${RELEASE_STATE:-enabled}" \
    EDGEZERO_CANDIDATE_PR_NUMBER="${PR_NUMBER:-17}" \
    EDGEZERO_CANDIDATE_HEAD_REPOSITORY="${HEAD_REPOSITORY:-stackpop/edgezero}" \
    EDGEZERO_CANDIDATE_HEAD_SHA="${HEAD_SHA:-$Q}" \
    bash "$ASSERT" --gate-root "${GATE_ROOT:-$REPO}"
}

run_assert_with_ambient_fsmonitor() {
  GIT_CONFIG_COUNT=1 \
    GIT_CONFIG_KEY_0=core.fsmonitor \
    GIT_CONFIG_VALUE_0="$FSMONITOR" \
    run_assert || return
  [[ ! -e "$FSMONITOR_SENTINEL" ]]
}

echo '== protected build-container dispatch context =='

write_reply
assert_pass 'later protected snapshot with unchanged gate bytes is accepted'
if [[ -f "$FAKE_BIN/config" && -f "$FAKE_BIN/args" ]] &&
  grep -qF 'Authorization: Bearer fixture-token' "$FAKE_BIN/config" &&
  ! grep -qF 'fixture-token' "$FAKE_BIN/args"; then
  ok 'API token is passed through config stdin and never argv'
else
  no 'API token is passed through config stdin and never argv'
fi
if awk 'previous == "--connect-timeout" && $0 == "10" { found = 1 } { previous = $0 } END { exit !found }' \
  "$FAKE_BIN/args"; then
  ok 'API lookup fixes the connection timeout at 10 seconds'
else
  no 'API lookup fixes the connection timeout at 10 seconds'
fi

SHA=$G
WORKFLOW_SHA=$G
HEAD_SHA=$G
write_reply stackpop/edgezero "$G"
assert_pass 'bootstrap snapshot Q equal to G is accepted'
clear_overrides
write_reply

# These fixed fixture assignments intentionally expand only when eval applies them.
# shellcheck disable=SC2016
for assignment in \
  'EVENT_NAME=pull_request' \
  'REPOSITORY=other/repository' \
  'REF=refs/heads/develop' \
  'REF_PROTECTED=false' \
  'WORKFLOW_SHA=$G' \
  'WORKFLOW_REF=stackpop/edgezero/.github/workflows/build-container-ci.yml@main' \
  'GATE_SHA=$Q' \
  'RELEASE_STATE=disabled:1:old' \
  'PR_NUMBER=0' \
  'HEAD_REPOSITORY=other/repository' \
  'HEAD_SHA=$G'; do
  eval "$assignment"
  assert_fail "dispatch rejects inconsistent input: $assignment"
  clear_overrides
done

write_reply other/repository "$Q"
assert_fail 'API head repository must match the exact input'
write_reply stackpop/edgezero "$G"
assert_fail 'API head SHA must match the exact input'
write_reply stackpop/edgezero "$Q" 302
assert_fail 'redirect response is rejected'
write_reply stackpop/edgezero "$Q" 200 2022-11-28
assert_fail 'wrong selected API version is rejected'
write_reply stackpop/edgezero "$Q" 200 2026-03-10 text/json
assert_fail 'wrong API media type is rejected'
write_reply

TOKEN=$'bad\ntoken'
assert_fail 'line-breaking token is rejected before curl'
clear_overrides

git -C "$REPO" switch -q -c changed-gate "$Q"
printf 'changed\n' >>"$REPO/.github/docker/build-app-cli/helper.sh"
git -C "$REPO" add .github/docker/build-app-cli/helper.sh
git -C "$REPO" commit -q -m changed-gate
CHANGED_Q=$(git -C "$REPO" rev-parse HEAD)
git -C "$REPO" checkout -q --detach "$G"
SHA=$CHANGED_Q
WORKFLOW_SHA=$CHANGED_Q
HEAD_SHA=$CHANGED_Q
write_reply stackpop/edgezero "$CHANGED_Q"
assert_fail 'changed gate-owned bytes at Q are rejected'
clear_overrides
write_reply

printf 'dirty\n' >"$REPO/dirty.txt"
assert_fail 'dirty gate checkout is rejected'
rm "$REPO/dirty.txt"

git -C "$REPO" replace "$G" "$Q"
assert_fail 'gate replacement refs are rejected'
git -C "$REPO" replace -d "$G"

GRAFTS="$(git -C "$REPO" rev-parse --absolute-git-dir)/info/grafts"
printf '%s\n' "$G" >"$GRAFTS"
assert_fail 'gate legacy grafts are rejected'
rm "$GRAFTS"

git -C "$REPO" checkout -q --detach "$Q"
assert_fail 'gate checkout HEAD must equal active G'
git -C "$REPO" checkout -q --detach "$G"

mkdir "$REPO/nested-root"
GATE_ROOT="$REPO/nested-root"
assert_fail_message 'gate root must be the exact repository top level' \
  'gate root must be the exact repository top level'
clear_overrides

FSMONITOR="$WORK/fsmonitor"
FSMONITOR_SENTINEL="$WORK/fsmonitor-ran"
{
  printf '#!/usr/bin/env bash\nset -euo pipefail\n'
  printf '%s\n' 'touch "$FAKE_FSMONITOR_SENTINEL"' 'printf "\\n"'
} >"$FSMONITOR"
chmod 0755 "$FSMONITOR"
export FAKE_FSMONITOR_SENTINEL=$FSMONITOR_SENTINEL
assert_pass 'ambient Git fsmonitor cannot execute in dispatch preflight' \
  run_assert_with_ambient_fsmonitor
unset FAKE_FSMONITOR_SENTINEL

ORIGINAL_REPO=$REPO
REPO="$WORK/linked-gate"
git -C "$ORIGINAL_REPO" worktree add -q --detach "$REPO" "$G"
ASSERT="$REPO/.github/docker/build-app-cli/assert-build-container-dispatch-context.sh"
COMMON_GIT_DIRECTORY=$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir)
printf '%s\n' "$G" >"$COMMON_GIT_DIRECTORY/info/grafts"
assert_fail 'linked-worktree common-directory grafts are rejected'

if ((fail)); then
  printf '\n%d passed, %d failed\n' "$pass" "$fail" >&2
  exit 1
fi
printf '\n%d passed, %d failed\n' "$pass" "$fail"
