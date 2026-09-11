#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REAL_DRIVER="$DIR/../../../docker/build-app-cli/run-build-container-gate.sh"
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
  local description=$1
  shift
  if "$@" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr"; then
    ok "$description"
  else
    cat "$CASE_ROOT/stderr" >&2
    no "$description"
  fi
}

assert_fail() {
  local description=$1
  shift
  if "$@" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr"; then
    cat "$CASE_ROOT/stdout" "$CASE_ROOT/stderr" "$LOG" >&2
    no "$description"
  elif [[ -e "$COMPLETION" || -L "$COMPLETION" ]]; then
    printf 'failed driver published a completion marker\n' >&2
    no "$description"
  else
    ok "$description"
  fi
}

write_executable() {
  local path=$1
  shift
  mkdir -p "$(dirname -- "$path")"
  {
    printf '#!/usr/bin/env bash\nset -euo pipefail\n'
    printf '%s\n' "$@"
  } >"$path"
  chmod 0755 "$path"
}

new_case() {
  unset CLASSIFIER_OUTPUT CLASSIFIER_STATUS CLASSIFIER_NUL FAIL_HELPER KIND ZERO_ACTION_REFS
  case_number=$((case_number + 1))
  CASE_ROOT="$WORK/case-$case_number"
  GATE="$CASE_ROOT/gate"
  SUBJECT="$CASE_ROOT/subject"
  RUN_WORK="$CASE_ROOT/run-work"
  COMPLETION="$RUN_WORK/completion"
  LOG="$CASE_ROOT/gate.log"
  FAKE_BIN="$CASE_ROOT/fake-bin"
  mkdir -p "$GATE/.github/docker/build-app-cli" "$FAKE_BIN" "$RUN_WORK"

  cp "$REAL_DRIVER" "$GATE/.github/docker/build-app-cli/run-build-container-gate.sh"
  chmod 0755 "$GATE/.github/docker/build-app-cli/run-build-container-gate.sh"
  printf 'FROM scratch\n' >"$GATE/.github/docker/build-app-cli/Dockerfile"
  printf '%s\n' \
    '.github/docker/build-app-cli/Dockerfile' \
    '.github/docker/build-app-cli/image-context-paths.txt' \
    >"$GATE/.github/docker/build-app-cli/image-context-paths.txt"

  write_executable "$GATE/.github/docker/build-app-cli/classify-build-container-change.sh" \
    'if [[ "${FAKE_CLASSIFIER_NUL:-false}" == true ]]; then printf "mode=ordinary\000\nrelevant=true\n"; exit 0; fi' \
    'printf "%s" "$FAKE_CLASSIFIER_OUTPUT"' \
    'exit "${FAKE_CLASSIFIER_STATUS:-0}"'
  write_executable "$GATE/.github/actions/deploy-core/tests/check-action-pins.sh" \
    'printf "action-pins:%s\n" "$*" >>"$FAKE_LOG"' \
    'if [[ "${FAKE_FAIL_HELPER:-}" == action-pins ]]; then exit 1; fi' \
    'for path in "$@"; do [[ "$path" == "$FAKE_RUN_WORK/"* && -f "$path" && ! -L "$path" ]]; done' \
    'if [[ "${FAKE_ZERO_ACTION_REFS:-false}" == true ]]; then count=0; else count=2; fi' \
    'printf "action reference policy passed (%s external references)\n" "$count"'
  write_executable "$GATE/.github/docker/build-app-cli/stage-build-context.sh" \
    'printf "stage:%s\n" "$*" >>"$FAKE_LOG"' \
    '[[ "${FAKE_FAIL_HELPER:-}" != stage ]] || exit 1' \
    'while (($#)); do if [[ "$1" == --output ]]; then output=$2; fi; shift; done' \
    'mkdir "$output"' \
    'mkdir -p "$output/.github/docker/build-app-cli"' \
    'cp "$FAKE_GATE_ROOT/.github/docker/build-app-cli/Dockerfile" "$output/.github/docker/build-app-cli/Dockerfile"'
  write_executable "$GATE/.github/docker/build-app-cli/assert-build-container-context.sh" \
    'printf "assert:%s\n" "$*" >>"$FAKE_LOG"' \
    '[[ "${FAKE_FAIL_HELPER:-}" != assert ]]'
  write_executable "$GATE/.github/docker/build-app-cli/verify-published-image.sh" \
    'printf "verify:%s\n" "$*" >>"$FAKE_LOG"' \
    '[[ "${FAKE_FAIL_HELPER:-}" != verify ]]'
  write_executable "$GATE/.github/docker/build-app-cli/check-build-container-publisher.sh" \
    'printf "publisher-structure:%s\n" "$*" >>"$FAKE_LOG"' \
    '[[ "${FAKE_FAIL_HELPER:-}" != publisher-structure ]]'
  write_executable "$GATE/.github/docker/build-app-cli/verify-build-container-publication.sh" \
    'printf "publisher-evidence:%s\n" "$*" >>"$FAKE_LOG"' \
    '[[ "${FAKE_FAIL_HELPER:-}" != publisher-evidence ]]'
  write_executable "$GATE/.github/docker/build-app-cli/check-image-pin.sh" \
    'printf "pin:%s\n" "$*" >>"$FAKE_LOG"' \
    '[[ "${FAKE_FAIL_HELPER:-}" != pin ]] || exit 1' \
    'case "$1" in' \
    '  runtime-ref) printf "%s\n" "ghcr.io/stackpop/edgezero-build-app-cli@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" ;;' \
    '  source-revision) printf "%s\n" "$FAKE_SOURCE_SHA" ;;' \
    '  provenance-protocol) printf "1\n" ;;' \
    'esac'
  write_executable "$FAKE_BIN/docker" \
    'printf "docker:%s\n" "$*" >>"$FAKE_LOG"' \
    '[[ "${FAKE_FAIL_HELPER:-}" != docker ]] || exit 1' \
    'while (($#)); do if [[ "$1" == --iidfile ]]; then iidfile=$2; fi; shift; done' \
    'printf "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n" >"$iidfile"'
  mkdir -p "$GATE/.github/workflows" "$GATE/tools/example"
  printf 'name: fixture\n' >"$GATE/.github/workflows/fixture.yml"
  printf 'runs:\n  using: composite\n  steps: []\n' >"$GATE/tools/example/action.yaml"

  git -C "$GATE" init -q -b main
  git -C "$GATE" config user.name fixture
  git -C "$GATE" config user.email fixture@example.invalid
  git -C "$GATE" add .
  git -C "$GATE" commit -q -m gate
  G=$(git -C "$GATE" rev-parse HEAD)

  git clone -q --no-hardlinks "$GATE" "$SUBJECT"
  git -C "$SUBJECT" config user.name fixture
  git -C "$SUBJECT" config user.email fixture@example.invalid
  printf 'candidate\n' >"$SUBJECT/app.txt"
  git -C "$SUBJECT" add app.txt
  git -C "$SUBJECT" commit -q -m candidate
  BASE=$G
  HEAD=$(git -C "$SUBJECT" rev-parse HEAD)
  DRIVER="$GATE/.github/docker/build-app-cli/run-build-container-gate.sh"
  : >"$LOG"
}

run_driver() {
  PATH="$FAKE_BIN:$PATH" \
    FAKE_LOG="$LOG" \
    FAKE_GATE_ROOT="$GATE" \
    FAKE_RUN_WORK="$RUN_WORK" \
    FAKE_SOURCE_SHA="$HEAD" \
    FAKE_CLASSIFIER_OUTPUT="${CLASSIFIER_OUTPUT:-$'mode=ordinary\nrelevant=true\n'}" \
    FAKE_CLASSIFIER_STATUS="${CLASSIFIER_STATUS:-0}" \
    FAKE_CLASSIFIER_NUL="${CLASSIFIER_NUL:-false}" \
    FAKE_FAIL_HELPER="${FAIL_HELPER:-}" \
    FAKE_ZERO_ACTION_REFS="${ZERO_ACTION_REFS:-false}" \
    bash "$DRIVER" \
      --subject-root "$SUBJECT" \
      --gate-root "$GATE" \
      --base "$BASE" \
      --head "$HEAD" \
      --kind "${KIND:-local}" \
      --gate-sha "$G" \
      --release-state enabled \
      --work-root "$RUN_WORK" \
      --completion-file "$COMPLETION"
}

run_driver_with_ambient_fsmonitor() {
  GIT_CONFIG_COUNT=1 \
    GIT_CONFIG_KEY_0=core.fsmonitor \
    GIT_CONFIG_VALUE_0="$FSMONITOR" \
    run_driver
  [[ ! -e "$FSMONITOR_SENTINEL" ]]
}

assert_completion() {
  local expected=$1 description=$2
  if [[ -f "$COMPLETION" && ! -L "$COMPLETION" ]] &&
    cmp -s <(printf '%s' "$expected") "$COMPLETION"; then
    ok "$description"
  else
    no "$description"
  fi
}

echo '== trusted build-container gate driver =='

new_case
assert_pass 'ordinary relevant local mode completes' run_driver
assert_completion $'kind=local\nmode=ordinary\nbranch=relevant\n' \
  'local completion marker is exact'
if grep -q '^publisher-structure:' "$LOG" && grep -q '^stage:' "$LOG" && grep -q '^docker:' "$LOG" &&
  grep -q '^verify:--local-image-id sha256:b\{64\} --source-sha ' "$LOG"; then
  ok 'local mode stages, builds by immutable ID, and verifies exact source'
else
  cat "$LOG" >&2
  no 'local mode stages, builds by immutable ID, and verifies exact source'
fi

new_case
CLASSIFIER_OUTPUT=$'mode=ordinary\nrelevant=false\n'
assert_pass 'ordinary not-applicable mode completes explicitly' run_driver
assert_completion $'kind=local\nmode=ordinary\nbranch=not-applicable\n' \
  'not-applicable completion marker is exact'
if grep -q '^action-pins:' "$LOG" && grep -q '^publisher-structure:' "$LOG" &&
  ! grep -Eq '^(stage|assert|docker|verify|pin|publisher-evidence):' "$LOG"; then
  ok 'not-applicable still runs both repository-wide static policy scans'
else
  cat "$LOG" >&2
  no 'not-applicable still runs both repository-wide static policy scans'
fi

new_case
CLASSIFIER_OUTPUT=$'mode=gate-update\nrelevant=true\n'
assert_pass 'gate update runs trusted static subject-data checks' run_driver
assert_completion $'kind=local\nmode=gate-update\nbranch=gate-update\n' \
  'gate-update completion marker is exact'
if grep -q '^assert:' "$LOG" &&
  grep -q "^publisher-structure:--gate-root $GATE --subject-root $SUBJECT --gate-sha $G --candidate-sha $HEAD$" "$LOG" &&
  ! grep -Eq '^(stage|docker|verify|publisher-evidence):' "$LOG"; then
  ok 'gate update runs only trusted static subject-data checks'
else
  cat "$LOG" >&2
  no 'gate update runs only trusted static subject-data checks'
fi

new_case
CLASSIFIER_OUTPUT=$'mode=gate-rollback\nrelevant=true\n'
assert_pass 'gate rollback runs trusted static restoration checks' run_driver
assert_completion $'kind=local\nmode=gate-rollback\nbranch=gate-rollback\n' \
  'gate-rollback completion marker is exact'
if grep -q '^publisher-structure:' "$LOG" && ! grep -q '^publisher-evidence:' "$LOG"; then
  ok 'gate rollback runs the structural publisher checker only'
else
  cat "$LOG" >&2
  no 'gate rollback runs the structural publisher checker only'
fi

new_case
KIND=pin
printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image.json"
printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image-release-evidence.json"
git -C "$SUBJECT" add .
git -C "$SUBJECT" commit -q -m pin
HEAD=$(git -C "$SUBJECT" rev-parse HEAD)
assert_pass 'ordinary relevant pin mode validates publisher and image' run_driver
assert_completion $'kind=pin\nmode=ordinary\nbranch=relevant\n' \
  'pin completion marker is exact'
if grep -q '^pin:validate-pair ' "$LOG" && grep -q '^publisher-evidence:' "$LOG" &&
  grep -q '^publisher-structure:' "$LOG" &&
  grep -q '^verify:--ref ghcr.io/stackpop/edgezero-build-app-cli@sha256:a\{64\}' "$LOG"; then
  ok 'pin mode validates the pair, publisher evidence, and digest reference'
else
  cat "$LOG" >&2
  no 'pin mode validates the pair, publisher evidence, and digest reference'
fi

for malformed in \
  $'mode=ordinary\n' \
  $'relevant=true\nmode=ordinary\n' \
  $'mode=ordinary\nrelevant=true\nrelevant=true\n' \
  $'mode=gate-update\nrelevant=false\n'; do
  new_case
  CLASSIFIER_OUTPUT=$malformed
  assert_fail 'malformed or contradictory classifier output fails closed' run_driver
done

new_case
CLASSIFIER_NUL=true
assert_fail 'NUL-bearing classifier output fails closed' run_driver

new_case
CLASSIFIER_STATUS=1
assert_fail 'classifier failure propagates without completion' run_driver

for helper in stage docker verify; do
  new_case
  FAIL_HELPER=$helper
  assert_fail "$helper failure propagates without completion" run_driver
done

new_case
FAIL_HELPER=action-pins
assert_fail 'action-reference policy failure propagates before classification' run_driver

new_case
ZERO_ACTION_REFS=true
assert_fail 'a vacuous action-reference scan fails before classification' run_driver

for helper in pin publisher-evidence verify; do
  new_case
  KIND=pin
  printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image.json"
  printf '{}\n' >"$SUBJECT/.github/docker/build-app-cli/image-release-evidence.json"
  git -C "$SUBJECT" add .
  git -C "$SUBJECT" commit -q -m pin
  HEAD=$(git -C "$SUBJECT" rev-parse HEAD)
  FAIL_HELPER=$helper
  assert_fail "$helper pin-path failure propagates without completion" run_driver
done

for mode in gate-update gate-rollback; do
  new_case
  CLASSIFIER_OUTPUT=$(printf 'mode=%s\nrelevant=true\n' "$mode")
  FAIL_HELPER=publisher-structure
  assert_fail "$mode structural publisher failure propagates without completion" run_driver
done

new_case
CLASSIFIER_OUTPUT=$'mode=ordinary\nrelevant=false\n'
FAIL_HELPER=publisher-structure
assert_fail 'ordinary structural publisher failure propagates without completion' run_driver

new_case
printf 'occupied\n' >"$COMPLETION"
if run_driver >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr"; then
  no 'a pre-existing completion marker is rejected'
elif cmp -s <(printf 'occupied\n') "$COMPLETION"; then
  ok 'a pre-existing completion marker is rejected without replacement'
else
  no 'a pre-existing completion marker is rejected without replacement'
fi

new_case
git -C "$SUBJECT" checkout -q "$BASE"
assert_fail 'subject checkout must be at the supplied full head SHA' run_driver

new_case
mkdir "$SUBJECT/nested-root"
SUBJECT="$SUBJECT/nested-root"
assert_fail 'subject root must be the exact repository top level' run_driver

new_case
ORIGINAL_SUBJECT=$SUBJECT
SUBJECT="$CASE_ROOT/linked-subject"
git -C "$GATE" worktree add -q -b candidate "$SUBJECT" "$G"
git -C "$SUBJECT" config user.name fixture
git -C "$SUBJECT" config user.email fixture@example.invalid
printf 'candidate\n' >"$SUBJECT/app.txt"
git -C "$SUBJECT" add app.txt
git -C "$SUBJECT" commit -q -m candidate
HEAD=$(git -C "$SUBJECT" rev-parse HEAD)
assert_fail 'gate and subject roots must use separate Git repositories' run_driver
SUBJECT=$ORIGINAL_SUBJECT

new_case
COMMON_GIT=$(git -C "$GATE" rev-parse --path-format=absolute --git-common-dir)
printf '%s\n' "$G" >"$COMMON_GIT/info/grafts"
assert_fail 'gate common-directory grafts are rejected' run_driver
rm "$COMMON_GIT/info/grafts"

new_case
FSMONITOR="$CASE_ROOT/fsmonitor"
FSMONITOR_SENTINEL="$CASE_ROOT/fsmonitor-ran"
write_executable "$FSMONITOR" \
  'touch "$FAKE_FSMONITOR_SENTINEL"' \
  'printf "\n"'
export FAKE_FSMONITOR_SENTINEL=$FSMONITOR_SENTINEL
assert_pass 'ambient Git fsmonitor cannot execute in the driver' run_driver_with_ambient_fsmonitor
unset FAKE_FSMONITOR_SENTINEL

new_case
printf 'candidate replacement\n' >"$SUBJECT/.github/docker/build-app-cli/verify-published-image.sh"
printf 'candidate scanner replacement\n' >"$SUBJECT/.github/actions/deploy-core/tests/check-action-pins.sh"
git -C "$SUBJECT" add .
git -C "$SUBJECT" commit -q -m candidate-helper
HEAD=$(git -C "$SUBJECT" rev-parse HEAD)
CLASSIFIER_OUTPUT=$'mode=gate-update\nrelevant=true\n'
assert_pass 'candidate helper substitution remains inert subject data' run_driver
if ! grep -q 'candidate replacement' "$LOG" && ! grep -q 'candidate scanner replacement' "$LOG" &&
  grep -q '^action-pins:' "$LOG" && grep -q '^assert:' "$LOG" &&
  grep -q '^publisher-structure:' "$LOG"; then
  ok 'only the separately checked-out gate helper executed'
else
  no 'only the separately checked-out gate helper executed'
fi

new_case
mkdir -p "$SUBJECT/tools/linked"
ln -s ../../app.txt "$SUBJECT/tools/linked/action.yml"
git -C "$SUBJECT" add tools/linked/action.yml
git -C "$SUBJECT" commit -q -m linked-action
HEAD=$(git -C "$SUBJECT" rev-parse HEAD)
assert_fail 'action-reference inventory rejects a symlink action metadata entry' run_driver

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
