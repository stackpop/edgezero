#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CLASSIFIER="$DIR/../../../docker/build-app-cli/classify-build-container-change.sh"
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
  local description=$1 expected=$2
  shift 2
  local actual="$CASE_ROOT/stdout" expected_file="$CASE_ROOT/expected" status=0
  printf '%s\n' "$expected" >"$expected_file"
  "$@" >"$actual" 2>"$CASE_ROOT/stderr" || status=$?
  if [[ "$status" -eq 0 ]] && cmp -s "$expected_file" "$actual" &&
    [[ ! -s "$CASE_ROOT/stderr" ]]; then
    ok "$description"
  else
    printf 'status: %s\nexpected bytes:\n' "$status" >&2
    od -An -tx1 "$expected_file" >&2
    printf 'actual bytes:\n' >&2
    od -An -tx1 "$actual" >&2
    printf 'stderr:\n' >&2
    cat "$CASE_ROOT/stderr" >&2
    no "$description"
  fi
}

assert_fail() {
  local description=$1
  shift
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

assert_fail_matching() {
  local description=$1 expected_error=$2
  shift 2
  if "$@" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr"; then
    no "$description"
  elif [[ -s "$CASE_ROOT/stdout" ]] || ! grep -Fq -- "$expected_error" "$CASE_ROOT/stderr"; then
    printf 'expected stderr to contain: %s\nactual stderr:\n' "$expected_error" >&2
    cat "$CASE_ROOT/stderr" >&2
    no "$description"
  else
    ok "$description"
  fi
}

write_base_gate_files() {
  mkdir -p .github/docker/build-app-cli .github
  printf 'fixture dockerignore\n' >.dockerignore
  printf 'FROM scratch\n' >.github/docker/build-app-cli/Dockerfile
  printf '#!/usr/bin/env bash\n' >.github/docker/build-app-cli/old-helper.sh
  printf '%s\n' \
    '.dockerignore' \
    '.github/docker/build-app-cli/Dockerfile' \
    '.github/docker/build-app-cli/image-context-paths.txt' \
    >.github/docker/build-app-cli/image-context-paths.txt
  printf '%s\n' \
    '.dockerignore' \
    '.github/CODEOWNERS' \
    '.github/docker/build-app-cli/Dockerfile' \
    '.github/docker/build-app-cli/gate-paths.txt' \
    '.github/docker/build-app-cli/image-context-paths.txt' \
    '.github/docker/build-app-cli/old-helper.sh' \
    >.github/docker/build-app-cli/gate-paths.txt
  write_codeowners
}

write_codeowners() {
  local manifest=.github/docker/build-app-cli/gate-paths.txt path
  : >.github/CODEOWNERS
  while IFS= read -r path; do
    printf '/%s @stackpop/edgezero-build-container-gate-reviewers\n' "$path"
  done <"$manifest" >>.github/CODEOWNERS
}

new_case() {
  case_number=$((case_number + 1))
  CASE_ROOT="$WORK/case-$case_number"
  SUBJECT="$CASE_ROOT/subject"
  GATE="$CASE_ROOT/gate"
  mkdir -p "$SUBJECT"
  git -C "$SUBJECT" init -q -b main
  git -C "$SUBJECT" config user.name fixture
  git -C "$SUBJECT" config user.email fixture@example.invalid
  git -C "$SUBJECT" config advice.addEmbeddedRepo false
  (
    cd "$SUBJECT"
    write_base_gate_files
    printf 'base\n' >app.txt
    git add .
    git commit -q -m base
  )
  G=$(git -C "$SUBJECT" rev-parse HEAD)
  BASE=$G
  git clone -q --no-hardlinks "$SUBJECT" "$GATE"
}

commit_subject() {
  local message=$1
  git -C "$SUBJECT" add -A
  git -C "$SUBJECT" commit -q -m "$message"
  HEAD=$(git -C "$SUBJECT" rev-parse HEAD)
}

reset_gate_to_subject_head() {
  G=$HEAD
  BASE=$G
  rm -rf "$GATE"
  git clone -q --no-hardlinks "$SUBJECT" "$GATE"
}

classify() {
  local kind=$1 release_state=${2:-enabled}
  bash "$CLASSIFIER" \
    --subject-root "$SUBJECT" \
    --gate-root "$GATE" \
    --base "$BASE" \
    --head "$HEAD" \
    --kind "$kind" \
    --gate-sha "$G" \
      --release-state "$release_state"
}

classify_with_ambient_fsmonitor() {
  GIT_CONFIG_COUNT=1 \
    GIT_CONFIG_KEY_0=core.fsmonitor \
    GIT_CONFIG_VALUE_0="$FSMONITOR" \
    classify local || return
  [[ ! -e "$FSMONITOR_SENTINEL" ]]
}

echo '== build-container change classifier =='

new_case
printf 'ordinary\n' >>"$SUBJECT/app.txt"
commit_subject ordinary
assert_output 'an unrelated ordinary change is explicit local not-applicable' \
  $'mode=ordinary\nrelevant=false' classify local
assert_output 'an unrelated ordinary change is explicit pin not-applicable' \
  $'mode=ordinary\nrelevant=false' classify pin

new_case
printf 'ordinary\n' >>"$SUBJECT/app.txt"
commit_subject ordinary
ORIGINAL_SUBJECT=$SUBJECT
mkdir "$SUBJECT/nested-root"
SUBJECT="$SUBJECT/nested-root"
assert_fail_matching 'subject root must be the exact repository top level' \
  'subject checkout must be the exact repository top level' classify local
SUBJECT=$ORIGINAL_SUBJECT

new_case
rm -rf "$GATE"
git -C "$SUBJECT" worktree add -q --detach "$GATE" "$G"
printf 'ordinary\n' >>"$SUBJECT/app.txt"
commit_subject ordinary
assert_fail 'gate and subject roots must use separate Git repositories' classify local

new_case
printf 'ordinary\n' >>"$SUBJECT/app.txt"
commit_subject ordinary
GATE_COMMON_DIRECTORY=$(git -C "$GATE" rev-parse --path-format=absolute --git-common-dir)
printf '%s\n' "$G" >"$GATE_COMMON_DIRECTORY/info/grafts"
assert_fail 'gate common-directory grafts are rejected' classify local

new_case
printf 'ordinary\n' >>"$SUBJECT/app.txt"
commit_subject ordinary
printf 'original object\n' >"$CASE_ROOT/original-object"
printf 'replacement object\n' >"$CASE_ROOT/replacement-object"
ORIGINAL_OBJECT=$(git -C "$SUBJECT" hash-object -w "$CASE_ROOT/original-object")
REPLACEMENT_OBJECT=$(git -C "$SUBJECT" hash-object -w "$CASE_ROOT/replacement-object")
git -C "$SUBJECT" replace "$ORIGINAL_OBJECT" "$REPLACEMENT_OBJECT"
assert_fail 'subject replacement refs are rejected even when unrelated to the range' classify local

new_case
printf 'ordinary\n' >>"$SUBJECT/app.txt"
commit_subject ordinary
FSMONITOR="$CASE_ROOT/fsmonitor"
FSMONITOR_SENTINEL="$CASE_ROOT/fsmonitor-ran"
{
  printf '#!/usr/bin/env bash\nset -euo pipefail\n'
  printf '%s\n' 'touch "$FAKE_FSMONITOR_SENTINEL"' 'printf "\\n"'
} >"$FSMONITOR"
chmod 0755 "$FSMONITOR"
export FAKE_FSMONITOR_SENTINEL=$FSMONITOR_SENTINEL
assert_output 'ambient Git fsmonitor cannot execute in the classifier' \
  $'mode=ordinary\nrelevant=false' classify_with_ambient_fsmonitor
unset FAKE_FSMONITOR_SENTINEL

new_case
printf '%s' \
  "{\"gate-sha\":\"$G\",\"provenance-protocol\":1,\"release-tag\":\"build-container-v1\"}" \
  >"$SUBJECT/.github/docker/build-app-cli/release-request.json"
commit_subject release-request
assert_output 'a sole canonical release request is local-image relevant' \
  $'mode=ordinary\nrelevant=true' classify local
assert_output 'a sole canonical release request is pin not-applicable' \
  $'mode=ordinary\nrelevant=false' classify pin

new_case
printf '%s' \
  "{\"gate-sha\":\"$G\",\"provenance-protocol\":1,\"release-tag\":\"build-container-v1\"}" \
  >"$SUBJECT/.github/docker/build-app-cli/release-request.json"
printf 'mixed\n' >>"$SUBJECT/app.txt"
commit_subject mixed-release
assert_fail 'a release request mixed with another path fails closed' classify local
assert_fail 'mixed release-request failure is independent of job kind' classify pin

new_case
printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image.json"
printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image-release-evidence.json"
commit_subject pin-pair
assert_output 'an exact added pin pair is pin relevant' \
  $'mode=ordinary\nrelevant=true' classify pin
assert_output 'an exact added pin pair is local-image not-applicable' \
  $'mode=ordinary\nrelevant=false' classify local

new_case
printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image.json"
commit_subject one-pin
assert_fail 'a one-sided pin addition fails closed' classify pin
assert_fail 'a one-sided pin addition cannot disappear in the local job' classify local

new_case
printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image.json"
printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image-release-evidence.json"
commit_subject pin-base
reset_gate_to_subject_head
rm "$SUBJECT/.github/docker/build-app-cli/image.json"
rm "$SUBJECT/.github/docker/build-app-cli/image-release-evidence.json"
commit_subject pin-delete
assert_fail 'paired pin deletion is relevant failure, never not-applicable' classify pin

new_case
printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image.json"
printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image-release-evidence.json"
commit_subject pin-base
reset_gate_to_subject_head
printf '{"next":true}\n' >"$SUBJECT/.github/docker/build-app-cli/image.json"
printf '{"next":true}\n' >"$SUBJECT/.github/docker/build-app-cli/image-release-evidence.json"
commit_subject pin-change
assert_output 'both pin records may advance together as one ordinary pin candidate' \
  $'mode=ordinary\nrelevant=true' classify pin

new_case
printf '# gate update\n' >>"$SUBJECT/.github/docker/build-app-cli/Dockerfile"
commit_subject gate-update
assert_output 'a manifested gate-only change is a relevant gate update' \
  $'mode=gate-update\nrelevant=true' classify local
assert_output 'gate update classification is identical in the pin job' \
  $'mode=gate-update\nrelevant=true' classify pin

new_case
printf '#!/usr/bin/env bash\n' >"$SUBJECT/.github/docker/build-app-cli/new-helper.sh"
printf '%s\n' \
  '.dockerignore' \
  '.github/CODEOWNERS' \
  '.github/docker/build-app-cli/Dockerfile' \
  '.github/docker/build-app-cli/gate-paths.txt' \
  '.github/docker/build-app-cli/image-context-paths.txt' \
  '.github/docker/build-app-cli/new-helper.sh' \
  '.github/docker/build-app-cli/old-helper.sh' \
  >"$SUBJECT/.github/docker/build-app-cli/gate-paths.txt"
(
  cd "$SUBJECT"
  write_codeowners
)
commit_subject gate-add
assert_output 'a newly manifested regular gate path is a gate update' \
  $'mode=gate-update\nrelevant=true' classify local

new_case
rm "$SUBJECT/.github/docker/build-app-cli/old-helper.sh"
printf '%s\n' \
  '.dockerignore' \
  '.github/CODEOWNERS' \
  '.github/docker/build-app-cli/Dockerfile' \
  '.github/docker/build-app-cli/gate-paths.txt' \
  '.github/docker/build-app-cli/image-context-paths.txt' \
  >"$SUBJECT/.github/docker/build-app-cli/gate-paths.txt"
(
  cd "$SUBJECT"
  write_codeowners
)
commit_subject gate-delete
assert_output 'a removed old-manifest path uses the old/candidate union' \
  $'mode=gate-update\nrelevant=true' classify pin

new_case
mv "$SUBJECT/.github/docker/build-app-cli/old-helper.sh" \
  "$SUBJECT/.github/docker/build-app-cli/renamed-helper.sh"
printf '%s\n' \
  '.dockerignore' \
  '.github/CODEOWNERS' \
  '.github/docker/build-app-cli/Dockerfile' \
  '.github/docker/build-app-cli/gate-paths.txt' \
  '.github/docker/build-app-cli/image-context-paths.txt' \
  '.github/docker/build-app-cli/renamed-helper.sh' \
  >"$SUBJECT/.github/docker/build-app-cli/gate-paths.txt"
(
  cd "$SUBJECT"
  write_codeowners
)
commit_subject gate-rename
assert_output 'a gate rename is classified through old-name deletion and new-name addition' \
  $'mode=gate-update\nrelevant=true' classify local

new_case
printf '# gate update\n' >>"$SUBJECT/.github/docker/build-app-cli/Dockerfile"
printf 'mixed\n' >>"$SUBJECT/app.txt"
commit_subject mixed-gate
assert_fail 'a gate and non-gate change fails closed' classify local

new_case
printf '%s\n' \
  '.github/CODEOWNERS' \
  '.dockerignore' \
  '.github/docker/build-app-cli/Dockerfile' \
  '.github/docker/build-app-cli/gate-paths.txt' \
  '.github/docker/build-app-cli/image-context-paths.txt' \
  '.github/docker/build-app-cli/old-helper.sh' \
  >"$SUBJECT/.github/docker/build-app-cli/gate-paths.txt"
commit_subject unsorted-manifest
assert_fail 'an unsorted candidate gate manifest fails closed' classify local

new_case
printf '%s\n' \
  '.dockerignore' \
  '.github/CODEOWNERS' \
  '.github/docker/build-app-cli/Dockerfile' \
  '.github/docker/build-app-cli/gate-paths.txt' \
  '.github/docker/build-app-cli/image-context-paths.txt' \
  '.github/docker/build-app-cli/missing-helper.sh' \
  '.github/docker/build-app-cli/old-helper.sh' \
  >"$SUBJECT/.github/docker/build-app-cli/gate-paths.txt"
(
  cd "$SUBJECT"
  write_codeowners
)
commit_subject missing-manifested-path
assert_fail 'a candidate manifest cannot claim a missing gate path' classify local

new_case
rm "$SUBJECT/.github/docker/build-app-cli/Dockerfile"
ln -s ../../../.dockerignore "$SUBJECT/.github/docker/build-app-cli/Dockerfile"
commit_subject symlinked-gate-path
assert_fail 'a manifested symlink is not a gate file' classify local

new_case
printf 'owner drift\n' >>"$SUBJECT/.github/CODEOWNERS"
commit_subject codeowners-drift
assert_fail 'candidate CODEOWNERS must be the exact manifest expansion' classify local

new_case
printf 'bad\n' >"$SUBJECT"/$'ambiguous\npath'
commit_subject newline-path
assert_fail 'a changed path containing a newline fails closed' classify local

new_case
git clone -q --no-hardlinks "$GATE" "$SUBJECT/vendor-dependency"
commit_subject gitlink
assert_fail 'a changed gitlink fails closed' classify local

new_case
printf '%s\n' \
  "{\"gate-sha\":\"$G\",\"provenance-protocol\":1,\"release-tag\":\"build-container-v1\"}" \
  >"$SUBJECT/.github/docker/build-app-cli/release-request.json"
commit_subject release-with-newline
assert_fail 'release-request bytes with a trailing newline fail closed' classify local

new_case
printf '# failed gate\n' >>"$SUBJECT/.github/docker/build-app-cli/Dockerfile"
commit_subject failed-gate
FAILED_G=$HEAD
BASE=$FAILED_G
git -C "$SUBJECT" checkout -q "$G" -- .github/docker/build-app-cli/Dockerfile
commit_subject rollback
assert_output 'the exact disabled failed-gate restoration is gate rollback' \
  $'mode=gate-rollback\nrelevant=true' \
  classify local "disabled:123:$G:$FAILED_G"
assert_fail 'the same restoration is forbidden while release is enabled' classify local enabled

new_case
printf '# failed gate\n' >>"$SUBJECT/.github/docker/build-app-cli/Dockerfile"
printf '#!/usr/bin/env bash\n' >"$SUBJECT/.github/docker/build-app-cli/new-helper.sh"
printf '%s\n' \
  '.dockerignore' \
  '.github/CODEOWNERS' \
  '.github/docker/build-app-cli/Dockerfile' \
  '.github/docker/build-app-cli/gate-paths.txt' \
  '.github/docker/build-app-cli/image-context-paths.txt' \
  '.github/docker/build-app-cli/new-helper.sh' \
  '.github/docker/build-app-cli/old-helper.sh' \
  >"$SUBJECT/.github/docker/build-app-cli/gate-paths.txt"
(
  cd "$SUBJECT"
  write_codeowners
)
commit_subject failed-gate-with-new-path
FAILED_G=$HEAD
BASE=$FAILED_G
git -C "$SUBJECT" checkout -q "$G" -- \
  .github/CODEOWNERS \
  .github/docker/build-app-cli/Dockerfile \
  .github/docker/build-app-cli/gate-paths.txt
rm "$SUBJECT/.github/docker/build-app-cli/new-helper.sh"
commit_subject rollback-removes-new-path
assert_output 'rollback may remove a path introduced only by failed G prime' \
  $'mode=gate-rollback\nrelevant=true' \
  classify pin "disabled:456:$G:$FAILED_G"
assert_fail 'rollback rejects a zero lock run id' \
  classify pin "disabled:0:$G:$FAILED_G"
assert_fail 'rollback rejects a state bound to a different failed gate' \
  classify pin "disabled:456:$G:1111111111111111111111111111111111111111"
assert_fail 'rollback rejects the interrupted state that lacks failed-G prime' \
  classify pin "disabled:456:$G"

new_case
printf '# drift\n' >>"$SUBJECT/.github/docker/build-app-cli/Dockerfile"
commit_subject drift
BASE=$HEAD
printf 'ordinary\n' >>"$SUBJECT/app.txt"
commit_subject after-drift
assert_fail 'ordinary work fails when the base gate bytes differ from active G' classify local

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
