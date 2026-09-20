#!/usr/bin/env bash
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SELECTOR="$DIR/../../../docker/build-app-cli/select-build-container-range.sh"
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

assert_output() {
  local description=$1 expected=$2 status=0
  shift 2
  CASE_ROOT="$WORK/case-$((case_number += 1))"
  mkdir -p "$CASE_ROOT"
  printf '%s\n' "$expected" >"$CASE_ROOT/expected"
  "$@" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$?
  if [[ "$status" -eq 0 && ! -s "$CASE_ROOT/stderr" ]] &&
    cmp -s "$CASE_ROOT/expected" "$CASE_ROOT/stdout"; then
    ok "$description"
  else
    printf 'status: %s\nexpected bytes:\n' "$status" >&2
    od -An -tx1 "$CASE_ROOT/expected" >&2
    printf 'actual bytes:\n' >&2
    od -An -tx1 "$CASE_ROOT/stdout" >&2
    cat "$CASE_ROOT/stderr" >&2
    no "$description"
  fi
}

assert_fail() {
  local description=$1
  shift
  CASE_ROOT="$WORK/case-$((case_number += 1))"
  mkdir -p "$CASE_ROOT"
  if "$@" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr"; then
    no "$description"
  elif [[ -s "$CASE_ROOT/stdout" ]]; then
    printf 'unexpected stdout:\n' >&2
    cat "$CASE_ROOT/stdout" >&2
    no "$description"
  else
    ok "$description"
  fi
}

REPO="$WORK/repository"
EVENT="$WORK/event.json"
mkdir -p "$REPO"
git -C "$REPO" init -q -b main
git -C "$REPO" config user.name fixture
git -C "$REPO" config user.email fixture@example.invalid
printf 'base\n' >"$REPO/base.txt"
git -C "$REPO" add base.txt
git -C "$REPO" commit -q -m base
A=$(git -C "$REPO" rev-parse HEAD)

git -C "$REPO" switch -q -c feature
printf 'feature\n' >"$REPO/feature.txt"
git -C "$REPO" add feature.txt
git -C "$REPO" commit -q -m feature
J=$(git -C "$REPO" rev-parse HEAD)

git -C "$REPO" switch -q main
printf 'first parent\n' >"$REPO/main.txt"
git -C "$REPO" add main.txt
git -C "$REPO" commit -q -m first-parent
F=$(git -C "$REPO" rev-parse HEAD)
git -C "$REPO" merge -q --no-ff feature -m synthetic-merge
M=$(git -C "$REPO" rev-parse HEAD)
G=$A

git -C "$REPO" switch -q --orphan unrelated
git -C "$REPO" rm -q -rf --ignore-unmatch .
printf 'unrelated\n' >"$REPO/unrelated.txt"
git -C "$REPO" add unrelated.txt
git -C "$REPO" commit -q -m unrelated
U=$(git -C "$REPO" rev-parse HEAD)
git -C "$REPO" switch -q main

GATE_REPO="$WORK/gate"
git clone -q --no-hardlinks "$REPO" "$GATE_REPO"
git -C "$GATE_REPO" checkout -q --detach "$G"

write_pr_event() {
  local base_sha=${1:-$A} head_sha=${2:-$J} number=${3:-17}
  local base_repo=${4:-stackpop/edgezero} base_ref=${5:-main}
  jq -cn \
    --arg base_sha "$base_sha" \
    --arg head_sha "$head_sha" \
    --arg base_repo "$base_repo" \
    --arg base_ref "$base_ref" \
    --argjson number "$number" \
    '{number:$number,pull_request:{base:{sha:$base_sha,ref:$base_ref,repo:{full_name:$base_repo}},head:{sha:$head_sha}}}' \
    >"$EVENT"
}

write_merge_group_event() {
  local base_sha=${1:-$F} head_sha=${2:-$M}
  local base_ref=${3:-refs/heads/main}
  local head_ref=${4:-refs/heads/gh-readonly-queue/main/pr-17-deadbeef}
  local action=${5:-checks_requested}
  jq -cn \
    --arg action "$action" \
    --arg base_sha "$base_sha" \
    --arg head_sha "$head_sha" \
    --arg base_ref "$base_ref" \
    --arg head_ref "$head_ref" \
    '{action:$action,merge_group:{base_sha:$base_sha,head_sha:$head_sha,base_ref:$base_ref,head_ref:$head_ref}}' \
    >"$EVENT"
}

write_push_event() {
  local before=${1:-$F} after=${2:-$M} repository=${3:-stackpop/edgezero}
  jq -cn \
    --arg before "$before" \
    --arg after "$after" \
    --arg repository "$repository" \
    '{before:$before,after:$after,repository:{full_name:$repository}}' \
    >"$EVENT"
}

reset_subject() {
  git -C "$REPO" switch -q main
  git -C "$REPO" reset -q --hard "$M"
  git -C "$REPO" clean -q -fd
}

run_selector() {
  bash "$SELECTOR" \
    --event-name "${EVENT_NAME:-pull_request}" \
    --github-repository "${GITHUB_REPOSITORY_VALUE:-stackpop/edgezero}" \
    --github-sha "${GITHUB_SHA_VALUE:-$M}" \
    --github-workflow-sha "${GITHUB_WORKFLOW_SHA_VALUE:-$G}" \
    --github-ref "${GITHUB_REF_VALUE:-refs/pull/17/merge}" \
    --github-ref-protected "${GITHUB_REF_PROTECTED_VALUE:-false}" \
    --gate-sha "${GATE_SHA_VALUE:-$G}" \
    --gate-root "${GATE_ROOT_VALUE:-$GATE_REPO}" \
    --event-json "${EVENT_VALUE:-$EVENT}" \
    --subject-root "${SUBJECT_ROOT_VALUE:-$REPO}"
}

clear_overrides() {
  unset EVENT_NAME GITHUB_REPOSITORY_VALUE GITHUB_SHA_VALUE GITHUB_WORKFLOW_SHA_VALUE
  unset GITHUB_REF_VALUE GITHUB_REF_PROTECTED_VALUE GATE_SHA_VALUE GATE_ROOT_VALUE
  unset EVENT_VALUE SUBJECT_ROOT_VALUE
}

echo '== trusted build-container event range selector =='

write_pr_event
assert_output 'pull request selects authenticated synthetic first-parent range' \
  "$(printf 'base=%s\nhead=%s' "$F" "$M")" run_selector

write_pr_event "$A" "$J"
GITHUB_WORKFLOW_SHA_VALUE=$F
assert_fail 'pull request workflow revision must be active gate G' run_selector
clear_overrides

write_pr_event "$A" "$J" 17 other/repository
assert_fail 'pull request base repository must be exact' run_selector
write_pr_event "$A" "$J" 17 stackpop/edgezero develop
assert_fail 'pull request base ref must be main' run_selector
write_pr_event "$A" "$J" 17
GITHUB_REF_VALUE=refs/pull/18/merge
assert_fail 'pull request merge ref must match canonical event number' run_selector
clear_overrides

write_pr_event "$A" "$F"
assert_fail 'pull request second parent must equal payload head' run_selector
write_pr_event "$U" "$J"
assert_fail 'pull request payload base must be an ancestor of first parent' run_selector
write_pr_event "$A" "$J" 0
assert_fail 'pull request number must be a positive integer' run_selector

WRONG_PARENT=$(git -C "$REPO" commit-tree "${M}^{tree}" -p "$F" -p "$U" <<<'wrong second parent')
git -C "$REPO" reset -q --hard "$WRONG_PARENT"
write_pr_event "$A" "$J"
GITHUB_SHA_VALUE=$WRONG_PARENT
assert_fail 'synthetic merge with wrong second parent is rejected' run_selector
clear_overrides
reset_subject

git -C "$REPO" reset -q --hard "$J"
write_pr_event "$A" "$J"
GITHUB_SHA_VALUE=$J
assert_fail 'pull request candidate must have exactly two parents' run_selector
clear_overrides
reset_subject

REVERSED=$(git -C "$REPO" commit-tree "${M}^{tree}" -p "$J" -p "$F" <<<'reversed parents')
git -C "$REPO" reset -q --hard "$REVERSED"
write_pr_event "$A" "$J"
GITHUB_SHA_VALUE=$REVERSED
assert_fail 'synthetic merge parent order is authenticated' run_selector
clear_overrides
reset_subject

write_pr_event "1111111111111111111111111111111111111111" "$J"
assert_fail 'missing pull request base object is rejected' run_selector
write_pr_event "$A" "2222222222222222222222222222222222222222"
assert_fail 'missing pull request head object is rejected' run_selector

write_merge_group_event
EVENT_NAME=merge_group
GITHUB_REF_VALUE=refs/heads/gh-readonly-queue/main/pr-17-deadbeef
assert_output 'merge group selects payload base and candidate' \
  "$(printf 'base=%s\nhead=%s' "$F" "$M")" run_selector
clear_overrides

write_merge_group_event "$F" "$M" refs/heads/main \
  refs/heads/gh-readonly-queue/main/pr-17-deadbeef completed
EVENT_NAME=merge_group
GITHUB_REF_VALUE=refs/heads/gh-readonly-queue/main/pr-17-deadbeef
assert_fail 'merge group action must be checks_requested' run_selector
clear_overrides

write_merge_group_event "$F" "$M" refs/heads/develop
EVENT_NAME=merge_group
GITHUB_REF_VALUE=refs/heads/gh-readonly-queue/main/pr-17-deadbeef
assert_fail 'merge group base ref must be exact main ref' run_selector
clear_overrides

write_merge_group_event "$F" "$M" refs/heads/main refs/heads/queue/main/pr-17
EVENT_NAME=merge_group
GITHUB_REF_VALUE=refs/heads/queue/main/pr-17
assert_fail 'merge group head ref must use exact queue prefix' run_selector
clear_overrides

write_merge_group_event
EVENT_NAME=merge_group
GITHUB_REF_VALUE=refs/heads/gh-readonly-queue/main/other
assert_fail 'merge group context ref must equal payload head ref' run_selector
clear_overrides

write_merge_group_event "$U" "$M"
EVENT_NAME=merge_group
GITHUB_REF_VALUE=refs/heads/gh-readonly-queue/main/pr-17-deadbeef
assert_fail 'merge group base must be an ancestor of candidate' run_selector
clear_overrides

write_merge_group_event "$F" "$M"
EVENT_NAME=merge_group
GITHUB_REF_VALUE=refs/heads/gh-readonly-queue/main/pr-17-deadbeef
GITHUB_WORKFLOW_SHA_VALUE=$F
assert_fail 'merge group workflow revision must be active gate G' run_selector
clear_overrides

write_push_event
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/main
GITHUB_REF_PROTECTED_VALUE=true
GITHUB_WORKFLOW_SHA_VALUE=$M
assert_output 'protected main push selects exact before and after range' \
  "$(printf 'base=%s\nhead=%s' "$F" "$M")" run_selector
clear_overrides

write_push_event "$F" "$M" other/repository
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/main
GITHUB_REF_PROTECTED_VALUE=true
GITHUB_WORKFLOW_SHA_VALUE=$M
assert_fail 'push payload repository must be exact' run_selector
clear_overrides

write_push_event 0000000000000000000000000000000000000000 "$M"
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/main
GITHUB_REF_PROTECTED_VALUE=true
GITHUB_WORKFLOW_SHA_VALUE=$M
assert_fail 'all-zero first-push base is rejected' run_selector
clear_overrides

write_push_event "$F" "$M"
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/develop
GITHUB_REF_PROTECTED_VALUE=true
GITHUB_WORKFLOW_SHA_VALUE=$M
assert_fail 'push ref must be exact protected main' run_selector
clear_overrides

write_push_event "$F" "$M"
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/main
GITHUB_REF_PROTECTED_VALUE=false
GITHUB_WORKFLOW_SHA_VALUE=$M
assert_fail 'unprotected main push is rejected' run_selector
clear_overrides

write_push_event "$F" "$M"
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/main
GITHUB_REF_PROTECTED_VALUE=true
GITHUB_WORKFLOW_SHA_VALUE=$F
assert_fail 'push workflow revision must equal candidate revision' run_selector
clear_overrides

write_push_event "$F" "$F"
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/main
GITHUB_REF_PROTECTED_VALUE=true
GITHUB_WORKFLOW_SHA_VALUE=$M
assert_fail 'push payload after must equal context SHA' run_selector
clear_overrides

write_push_event "$U" "$M"
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/main
GITHUB_REF_PROTECTED_VALUE=true
GITHUB_WORKFLOW_SHA_VALUE=$M
assert_fail 'push before must be an ancestor of after' run_selector
clear_overrides

write_pr_event
EVENT_NAME=workflow_dispatch
assert_fail 'workflow dispatch is never a range-consumer event' run_selector
clear_overrides
EVENT_NAME=pull_request_target
assert_fail 'every other event is rejected before classification' run_selector
clear_overrides

printf '{not-json\n' >"$EVENT"
assert_fail 'malformed event JSON is rejected' run_selector
write_pr_event

git -C "$REPO" status --porcelain >/dev/null
printf 'dirty\n' >>"$REPO/base.txt"
assert_fail 'dirty subject checkout is rejected' run_selector
git -C "$REPO" reset -q --hard "$M"

SHALLOW="$WORK/shallow"
git clone -q --depth 1 "file://$REPO" "$SHALLOW"
SUBJECT_ROOT_VALUE=$SHALLOW
assert_fail 'shallow subject checkout is rejected' run_selector
clear_overrides

git -C "$REPO" replace "$F" "$A"
assert_fail 'replacement refs are rejected' run_selector
git -C "$REPO" replace -d "$F"

GRAFTS="$(git -C "$REPO" rev-parse --absolute-git-dir)/info/grafts"
printf '%s %s\n' "$M" "$F" >"$GRAFTS"
assert_fail 'legacy grafts are rejected' run_selector
rm -f "$GRAFTS"

GITHUB_REPOSITORY_VALUE=other/repository
assert_fail 'workflow repository must be exact' run_selector
clear_overrides

GITHUB_SHA_VALUE=$F
assert_fail 'subject HEAD must equal the supplied context SHA' run_selector
clear_overrides

git -C "$GATE_REPO" checkout -q --detach "$F"
assert_fail 'gate checkout HEAD must equal active gate G' run_selector
git -C "$GATE_REPO" checkout -q --detach "$G"

mkdir "$REPO/nested-root"
SUBJECT_ROOT_VALUE="$REPO/nested-root"
assert_fail 'subject input must be the exact repository top level' run_selector
clear_overrides
rmdir "$REPO/nested-root"

SAME_GATE="$WORK/same-common-gate"
git -C "$REPO" worktree add -q --detach "$SAME_GATE" "$G"
GATE_ROOT_VALUE="$SAME_GATE"
assert_fail 'gate and subject roots must have separate Git common directories' run_selector
clear_overrides
git -C "$REPO" worktree remove -f "$SAME_GATE"

LINKED_SUBJECT="$WORK/linked-subject"
git -C "$REPO" worktree add -q --detach "$LINKED_SUBJECT" "$M"
COMMON_GIT=$(git -C "$LINKED_SUBJECT" rev-parse --path-format=absolute --git-common-dir)
printf '%s %s\n' "$M" "$U" >"$COMMON_GIT/info/grafts"
write_push_event "$U" "$M"
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/main
GITHUB_REF_PROTECTED_VALUE=true
GITHUB_WORKFLOW_SHA_VALUE=$M
SUBJECT_ROOT_VALUE=$LINKED_SUBJECT
assert_fail 'linked-worktree common-directory grafts are rejected' run_selector
clear_overrides
rm "$COMMON_GIT/info/grafts"
git -C "$REPO" worktree remove -f "$LINKED_SUBJECT"

printf '{"before":"%s","after":"%s","repository":{"full_name":"stackpop/edgezero\\n"}}\n' \
  "$F" "$M" >"$EVENT"
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/main
GITHUB_REF_PROTECTED_VALUE=true
GITHUB_WORKFLOW_SHA_VALUE=$M
assert_fail 'line-breaking event strings cannot normalize to trusted values' run_selector
clear_overrides

printf '{"before":"%s","after":"%s","repository":{"full_name":"stackpop/edgezero\\u0000"}}\n' \
  "$F" "$M" >"$EVENT"
EVENT_NAME=push
GITHUB_REF_VALUE=refs/heads/main
GITHUB_REF_PROTECTED_VALUE=true
GITHUB_WORKFLOW_SHA_VALUE=$M
assert_fail 'NUL-bearing event strings cannot normalize to trusted values' run_selector
clear_overrides

write_pr_event
FSMONITOR_SENTINEL="$WORK/fsmonitor-ran"
cat >"$WORK/fsmonitor" <<EOF
#!/usr/bin/env bash
printf ran >"$FSMONITOR_SENTINEL"
printf '\\n'
EOF
chmod 0755 "$WORK/fsmonitor"
run_with_ambient_fsmonitor() {
  rm -f "$FSMONITOR_SENTINEL"
  GIT_CONFIG_COUNT=1 \
    GIT_CONFIG_KEY_0=core.fsmonitor \
    GIT_CONFIG_VALUE_0="$WORK/fsmonitor" \
    run_selector
  [[ ! -e "$FSMONITOR_SENTINEL" ]]
}
assert_output 'ambient Git config cannot execute a filesystem monitor' \
  "$(printf 'base=%s\nhead=%s' "$F" "$M")" run_with_ambient_fsmonitor

if ((fail)); then
  printf '\n%d passed, %d failed\n' "$pass" "$fail" >&2
  exit 1
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
