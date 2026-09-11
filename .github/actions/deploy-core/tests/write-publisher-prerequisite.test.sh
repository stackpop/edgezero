#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
WRITE="$DIR/../../../docker/build-app-cli/write-publisher-prerequisite.sh"

if [[ ! -x "$WRITE" ]]; then
  printf 'FAIL: missing executable helper: %s\n' "$WRITE" >&2
  exit 1
fi

WORK=$(mktemp -d)
WORK=$(cd -- "$WORK" && pwd -P)
trap 'rm -rf -- "$WORK"' EXIT

TOKEN=writer-token-secret-value
TOKEN_ID=18446744073709551615
SOURCE_PR=347
EVIDENCE_COMMENT_ID=9007199254740993
REAL_GIT=$(command -v git)
REAL_JQ=$(command -v jq)
NOW=2026-09-10T12:00:00Z
REVIEWED_AT=2026-09-10T11:59:00Z
EXPIRES_AT=2026-09-10T12:30:00Z
ROTATION_AT=2026-09-10T11:00:00Z
ROTATION_DIGEST="sha256:$(printf '8%.0s' {1..64})"
HISTORY_DIGEST="sha256:$(printf '7%.0s' {1..64})"
VARIABLE=EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE
API=https://api.github.com

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

hash_file() {
  sha256sum "$1" | awk '{print $1}'
}

hash_bytes() {
  printf '%s' "$1" | sha256sum | awk '{print $1}'
}

GATE_ROOT="$WORK/gate"
FAKE_BIN="$WORK/fake-bin"
INPUT_ROOT="$WORK/inputs"
mkdir -p "$GATE_ROOT/.github/docker/build-app-cli" "$FAKE_BIN" "$INPUT_ROOT"
git -C "$GATE_ROOT" init -q -b main
git -C "$GATE_ROOT" config user.name fixture
git -C "$GATE_ROOT" config user.email fixture@example.invalid
printf base >"$GATE_ROOT/base"
git -C "$GATE_ROOT" add base
git -C "$GATE_ROOT" commit -q -m base
OLD_G=$(git -C "$GATE_ROOT" rev-parse HEAD)
cp "$WRITE" "$GATE_ROOT/.github/docker/build-app-cli/write-publisher-prerequisite.sh"
chmod 0755 "$GATE_ROOT/.github/docker/build-app-cli/write-publisher-prerequisite.sh"
git -C "$GATE_ROOT" add .github/docker/build-app-cli/write-publisher-prerequisite.sh
git -C "$GATE_ROOT" commit -q -m gate
G=$(git -C "$GATE_ROOT" rev-parse HEAD)
printf one >"$GATE_ROOT/source"
git -C "$GATE_ROOT" add source
git -C "$GATE_ROOT" commit -q -m source-one
S1=$(git -C "$GATE_ROOT" rev-parse HEAD)
printf two >"$GATE_ROOT/source"
git -C "$GATE_ROOT" commit -qam source-two
S2=$(git -C "$GATE_ROOT" rev-parse HEAD)
git -C "$GATE_ROOT" checkout -q --detach "$G"
printf side >"$GATE_ROOT/side"
git -C "$GATE_ROOT" add side
git -C "$GATE_ROOT" commit -q -m side
SIDE=$(git -C "$GATE_ROOT" rev-parse HEAD)
git -C "$GATE_ROOT" checkout -q --detach "$G"
WRITE="$GATE_ROOT/.github/docker/build-app-cli/write-publisher-prerequisite.sh"

cat >"$FAKE_BIN/date" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=$(cd -- "$(dirname -- "$0")" && pwd)
[[ "$#" -eq 2 && "$1" == -u && "$2" == +%s ]]
[[ "${LC_ALL:-}" == C ]]
[[ -z "${EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN+x}${AMBIENT_SECRET+x}${HOME+x}" ]]
cat "$fixture/now-epoch"
SH
chmod 0755 "$FAKE_BIN/date"
jq -nr --arg value "$NOW" '$value | fromdateiso8601' >"$FAKE_BIN/now-epoch"

cat >"$FAKE_BIN/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=$(cd -- "$(dirname -- "$0")" && pwd)
[[ "${LC_ALL:-}" == C ]]
[[ -z "${EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN+x}${GITHUB_TOKEN+x}${GH_TOKEN+x}" ]]
[[ -z "${AMBIENT_SECRET+x}${HOME+x}${CURL_HOME+x}${XDG_CONFIG_HOME+x}" ]]
count=0
[[ ! -f "$fixture/curl-count" ]] || count=$(<"$fixture/curl-count")
count=$((count + 1))
printf '%s' "$count" >"$fixture/curl-count"
printf '%s\n' "$@" >"$fixture/args-$count"
cat >"$fixture/config-$count"

request= output= data= url=
while (($#)); do
  case "$1" in
    --request) request=$2; shift 2 ;;
    --output) output=$2; shift 2 ;;
    --data-binary) data=$2; shift 2 ;;
    --disable | --silent | --show-error) shift ;;
    --connect-timeout | --max-time | --max-redirs | --config | --write-out) shift 2 ;;
    *) url=$1; shift ;;
  esac
done
SH

run_remaining_tests() {

for invalid in review-duplicate review-reordered wrong-grants wrong-selection self-review \
  invalid-review-time future-review expired-review wrong-png-digest bad-png small-png large-png \
  empty-evidence large-evidence wrong-evidence-digest prereq-duplicate prereq-reordered \
  prereq-wrong-type schema-1 partial-url partial-pr partial-source url-pr-mismatch source-pr-zero \
  source-pr-leading-zero source-pr-overflow comment-id-zero comment-id-overflow \
  nested-duplicate nested-reordered nested-missing \
  numeric-token-id numeric-run-id \
  run-attempt-zero run-attempt-overflow run-id-zero rotation-overflow run-number-zero \
  run-number-overflow invalid-rotation-time \
  review-trailing prereq-trailing \
  symlink-review symlink-png symlink-evidence symlink-prereq; do
  new_case
  case "$invalid" in
    review-duplicate)
      sed 's/"token-id":/"token-id":7,"token-id":/' "$REVIEW_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$REVIEW_JSON"
      ;;
    review-reordered)
      jq -c '{"schema-version":."schema-version","expires-at":."expires-at","organization-grants":."organization-grants","repository-grants":."repository-grants","resource-owner":."resource-owner","reviewed-at":."reviewed-at","reviewer-login":."reviewer-login","screenshot-sha256":."screenshot-sha256","selected-repositories":."selected-repositories","subject-login":."subject-login","token-id":."token-id"}' \
        "$REVIEW_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$REVIEW_JSON"
      ;;
    wrong-grants)
      sed 's/"variables":"write"/"variables":"read"/' "$REVIEW_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$REVIEW_JSON"
      ;;
    wrong-selection)
      sed 's#stackpop/edgezero#stackpop/other#' "$REVIEW_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$REVIEW_JSON"
      ;;
    self-review) review_json "$REVIEWED_AT" "$EXPIRES_AT" variable-writer variable-writer >"$REVIEW_JSON" ;;
    invalid-review-time) review_json 2026-02-30T12:00:00Z >"$REVIEW_JSON" ;;
    future-review) review_json 2026-09-10T12:00:01Z >"$REVIEW_JSON" ;;
    expired-review) review_json "$REVIEWED_AT" "$NOW" >"$REVIEW_JSON" ;;
    wrong-png-digest) printf x >>"$PNG" ;;
    bad-png) printf 'not-a-png' >"$PNG" ;;
    small-png) printf 1234567 >"$PNG" ;;
    large-png) dd if=/dev/zero of="$PNG" bs=10485761 count=1 2>/dev/null ;;
    empty-evidence) : >"$EVIDENCE_JSON" ;;
    large-evidence) dd if=/dev/zero of="$EVIDENCE_JSON" bs=1048577 count=1 2>/dev/null ;;
    wrong-evidence-digest)
      set_requested "${REQUESTED/$EVIDENCE_DIGEST/sha256:$(printf 'f%.0s' {1..64})}"
      ;;
    prereq-duplicate)
      sed 's/"schema-version":2/"schema-version":2,"schema-version":2/' "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$PREREQUISITE_JSON"
      ;;
    prereq-reordered)
      jq -c '{"gate-sha":."gate-sha","evidence-sha256":."evidence-sha256","evidence-url":."evidence-url","previous-value-sha256":."previous-value-sha256","rotation-history":."rotation-history","schema-version":."schema-version","source-pr":."source-pr","source-revision":."source-revision"}' \
        "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$PREREQUISITE_JSON"
      ;;
    prereq-wrong-type)
      sed 's/"schema-version":2/"schema-version":"2"/' "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$PREREQUISITE_JSON"
      ;;
    schema-1)
      sed 's/"schema-version":2/"schema-version":1/' "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$PREREQUISITE_JSON"
      ;;
    partial-url | partial-pr | partial-source | url-pr-mismatch | source-pr-zero | \
      source-pr-leading-zero | source-pr-overflow | comment-id-zero | comment-id-overflow)
      case "$invalid" in
        partial-url) jq -c '."evidence-url" = null' "$PREREQUISITE_JSON" >"$CASE_ROOT/x" ;;
        partial-pr) jq -c '."source-pr" = null' "$PREREQUISITE_JSON" >"$CASE_ROOT/x" ;;
        partial-source) jq -c '."source-revision" = null' "$PREREQUISITE_JSON" >"$CASE_ROOT/x" ;;
        url-pr-mismatch)
          jq -c '."evidence-url" = "https://github.com/stackpop/edgezero/pull/999#issuecomment-9007199254740993"' \
            "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
          ;;
        source-pr-zero) jq -c '."source-pr" = "0"' "$PREREQUISITE_JSON" >"$CASE_ROOT/x" ;;
        source-pr-leading-zero) jq -c '."source-pr" = "0347"' "$PREREQUISITE_JSON" >"$CASE_ROOT/x" ;;
        source-pr-overflow)
          jq -c '."source-pr" = "18446744073709551616" | ."evidence-url" = "https://github.com/stackpop/edgezero/pull/18446744073709551616#issuecomment-9007199254740993"' \
            "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
          ;;
        comment-id-zero)
          jq -c '."evidence-url" = "https://github.com/stackpop/edgezero/pull/347#issuecomment-0"' \
            "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
          ;;
        comment-id-overflow)
          jq -c '."evidence-url" = "https://github.com/stackpop/edgezero/pull/347#issuecomment-18446744073709551616"' \
            "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
          ;;
      esac
      mv "$CASE_ROOT/x" "$PREREQUISITE_JSON"
      ;;
    nested-duplicate)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 1)"
      sed 's/"run-number":"10"/"run-number":"10","run-number":"10"/' \
        "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$PREREQUISITE_JSON"
      ;;
    nested-reordered)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 1)"
      jq -c '."rotation-history" |= {"state":.state,"created-at":."created-at","evidence-sha256":."evidence-sha256","history-sha256":."history-sha256","run-attempt":."run-attempt","run-id":."run-id","run-number":."run-number"}' \
        "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$PREREQUISITE_JSON"
      ;;
    nested-missing)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 1)"
      jq -c 'del(."rotation-history"."history-sha256")' "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$PREREQUISITE_JSON"
      ;;
    numeric-token-id)
      sed 's/"token-id":"18446744073709551615"/"token-id":18446744073709551615/' \
        "$REVIEW_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$REVIEW_JSON"
      ;;
    numeric-run-id)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 1)"
      sed 's/"run-id":"10"/"run-id":10/' "$PREREQUISITE_JSON" >"$CASE_ROOT/x"
      mv "$CASE_ROOT/x" "$PREREQUISITE_JSON"
      ;;
    run-attempt-zero)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 0)"
      ;;
    run-attempt-overflow)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 4294967296)"
      ;;
    run-id-zero)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 0 1)"
      ;;
    rotation-overflow)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 18446744073709551616 1)"
      ;;
    run-number-overflow)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 1 \
        "$ROTATION_AT" "$ROTATION_DIGEST" 18446744073709551616)"
      ;;
    run-number-zero)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 1 \
        "$ROTATION_AT" "$ROTATION_DIGEST" 0)"
      ;;
    invalid-rotation-time)
      previous="\"sha256:$(hash_bytes "$CURRENT")\""
      set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 1 \
        2026-02-30T11:00:00Z)"
      ;;
    review-trailing) printf '\n' >>"$REVIEW_JSON" ;;
    prereq-trailing) printf '\n' >>"$PREREQUISITE_JSON" ;;
    symlink-review) mv "$REVIEW_JSON" "$CASE_ROOT/real-review"; ln -s "$CASE_ROOT/real-review" "$REVIEW_JSON" ;;
    symlink-png) mv "$PNG" "$CASE_ROOT/real-png"; ln -s "$CASE_ROOT/real-png" "$PNG" ;;
    symlink-evidence) mv "$EVIDENCE_JSON" "$CASE_ROOT/real-evidence"; ln -s "$CASE_ROOT/real-evidence" "$EVIDENCE_JSON" ;;
    symlink-prereq) mv "$PREREQUISITE_JSON" "$CASE_ROOT/real-prereq"; ln -s "$CASE_ROOT/real-prereq" "$PREREQUISITE_JSON" ;;
  esac
  assert_result 1 "$invalid input is rejected" run_writer
  assert_no_patch "$invalid input fails before PATCH"
done

for path_failure in relative-input noncanonical-input noncanonical-gate; do
  new_case
  case "$path_failure" in
    relative-input) EVIDENCE_JSON=${EVIDENCE_JSON#/} ;;
    noncanonical-input) EVIDENCE_JSON="$CASE_ROOT/../case-$case_number/evidence.json" ;;
    noncanonical-gate) CLI_GATE_ROOT="$GATE_ROOT/../gate" ;;
  esac
  assert_result 1 "$path_failure path is rejected" run_writer
  assert_no_patch "$path_failure path fails before PATCH"
done

for api_failure in wrong-user inactive-member gate-mismatch malformed-current current-404 \
  get-status get-version get-media transport; do
  new_case
  case "$api_failure" in
    wrong-user) jq -cn --argjson id "$TOKEN_ID" '{login:"other",id:$id}' >"$FAKE_BIN/user.body" ;;
    inactive-member) jq -cn '{state:"pending",user:{login:"variable-writer"}}' >"$FAKE_BIN/membership.body" ;;
    gate-mismatch) jq -cn --arg value "$OLD_G" '{name:"EDGEZERO_BUILD_CONTAINER_GATE_SHA",value:$value}' >"$FAKE_BIN/gate.body" ;;
    malformed-current) write_variable_body "$FAKE_BIN/before.body" '{}' ;;
    current-404) set_metadata before 404 ;;
    get-status) set_metadata user 302 ;;
    get-version) set_metadata user 200 2022-11-28 ;;
    get-media) set_metadata user 200 2026-03-10 text/json ;;
    transport) touch "$FAKE_BIN/user.transport-failure" ;;
  esac
  assert_result 1 "$api_failure API state is rejected" run_writer
  assert_no_patch "$api_failure API failure does not PATCH"
done

new_case
set_metadata patch 200
assert_result 1 'PATCH requires HTTP 204' run_writer
if [[ $(patch_calls) == 1 && $(curl_calls) == 5 ]]; then
  ok 'failed PATCH is not retried or read back'
else
  no 'failed PATCH is not retried or read back'
fi

new_case
printf x >"$FAKE_BIN/patch.body"
assert_result 1 'PATCH requires an empty response body' run_writer

new_case
write_variable_body "$FAKE_BIN/after.body" "$CURRENT"
assert_result 1 'post-write readback must equal requested exact bytes' run_writer
if [[ $(patch_calls) == 1 && $(curl_calls) == 6 ]]; then
  ok 'readback failure follows exactly one PATCH'
else
  no 'readback failure follows exactly one PATCH'
fi

for exact_bytes in current-trailing-lf current-nul readback-trailing-lf readback-nul; do
  new_case
  if [[ "$exact_bytes" == current-* ]]; then
    current=$(bootstrap_record "sha256:$(printf '1%.0s' {1..64})" "$G" null null)
    set_current "$current"
    set_requested "$current"
    if [[ "$exact_bytes" == current-trailing-lf ]]; then
      write_variable_body_control "$FAKE_BIN/before.body" "$current" lf
    else
      write_variable_body_control "$FAKE_BIN/before.body" "$current" nul
    fi
    assert_result 1 "$exact_bytes is not normalized into idempotent success" run_writer
    assert_no_patch "$exact_bytes fails before PATCH"
  else
    if [[ "$exact_bytes" == readback-trailing-lf ]]; then
      write_variable_body_control "$FAKE_BIN/after.body" "$REQUESTED" lf
    else
      write_variable_body_control "$FAKE_BIN/after.body" "$REQUESTED" nul
    fi
    assert_result 1 "$exact_bytes cannot satisfy exact readback" run_writer
    if [[ $(patch_calls) == 1 ]]; then
      ok "$exact_bytes follows exactly one PATCH"
    else
      no "$exact_bytes follows exactly one PATCH"
    fi
  fi
done

new_case
jq -cn '{login:"variable-writer",id:42}' >"$FAKE_BIN/user.body"
assert_result 0 'review token inventory id is not compared with authenticated user id' run_writer

new_case
: >"$FAKE_BIN/patch.media"
assert_result 0 'PATCH accepts HTTP 204 with absent Content-Type' run_writer

for mode in unknown duplicate missing empty; do
  new_case
  status=0
  case "$mode" in
    unknown) run_writer --unknown value >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$? ;;
    duplicate) run_writer --gate-sha "$G" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$? ;;
    missing)
      PATH="$FAKE_BIN:$PATH" EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN="$TOKEN" bash "$WRITE" \
        --gate-root "$GATE_ROOT" --gate-sha "$G" --evidence-json "$EVIDENCE_JSON" \
        --publisher-prerequisite-json "$PREREQUISITE_JSON" --writer-token-review-json "$REVIEW_JSON" \
        >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$?
      ;;
    empty) CLI_GATE_SHA=; run_writer >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$? ;;
  esac
  if [[ "$status" -eq 2 && ! -s "$CASE_ROOT/stdout" && -s "$CASE_ROOT/stderr" ]]; then
    ok "$mode flags are rejected as usage"
  else
    no "$mode flags are rejected as usage"
  fi
  assert_no_patch "$mode usage failure does not PATCH"
done

new_case
mkdir "$CASE_ROOT/empty-path"
status=0
PATH="$CASE_ROOT/empty-path" EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN="$TOKEN" /bin/bash "$WRITE" \
  --gate-root "$GATE_ROOT" --gate-sha "$G" --evidence-json "$EVIDENCE_JSON" \
  --publisher-prerequisite-json "$PREREQUISITE_JSON" --writer-token-review-json "$REVIEW_JSON" \
  --writer-token-review-png "$PNG" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$?
if [[ "$status" -eq 2 ]]; then ok 'missing required tooling exits 2'; else no 'missing required tooling exits 2'; fi

new_case
assert_result 0 'inherited xtrace and allexport are disabled before secret expansion' run_writer_with_shellopts

new_case
GUARD_BIN="$CASE_ROOT/guard-jq"
mkdir "$GUARD_BIN"
printf '%s\n' "$REAL_JQ" >"$GUARD_BIN/real-jq"
cat >"$GUARD_BIN/jq" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ -z "${EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN+x}${TOKEN+x}${WRITER_TOKEN+x}${AMBIENT_SECRET+x}" ]]
IFS= read -r real_jq <"${0%/*}/real-jq"
exec "$real_jq" "$@"
SH
chmod 0755 "$GUARD_BIN/jq"
assert_result 0 'JSON subprocesses receive no credential or ambient secret' run_writer_with_path "$GUARD_BIN"

new_case
GUARD_BIN="$CASE_ROOT/guard-git"
mkdir "$GUARD_BIN"
printf '%s\n' "$REAL_GIT" >"$GUARD_BIN/real-git"
cat >"$GUARD_BIN/git" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "${GIT_NO_LAZY_FETCH:-}" == 1 ]]
[[ -z "${EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN+x}${TOKEN+x}${WRITER_TOKEN+x}${AMBIENT_SECRET+x}" ]]
IFS= read -r real_git <"${0%/*}/real-git"
exec "$real_git" "$@"
SH
chmod 0755 "$GUARD_BIN/git"
assert_result 0 'Git subprocesses disable lazy fetch and receive no credential' run_writer_with_path "$GUARD_BIN"

new_case
status=0
PATH="$FAKE_BIN:$PATH" AMBIENT_SECRET=must-not-reach-tools bash "$WRITE" \
  --gate-root "$GATE_ROOT" --gate-sha "$G" --evidence-json "$EVIDENCE_JSON" \
  --publisher-prerequisite-json "$PREREQUISITE_JSON" --writer-token-review-json "$REVIEW_JSON" \
  --writer-token-review-png "$PNG" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$?
if [[ "$status" -eq 1 && $(curl_calls) == 0 ]]; then ok 'missing credential is rejected before subprocess validation'; else no 'missing credential is rejected before subprocess validation'; fi

new_case
TOKEN=$'unsafe\ntoken'
assert_result 1 'unsafe credential is rejected without disclosure' run_writer
assert_no_patch 'unsafe credential does not PATCH'
TOKEN=writer-token-secret-value

new_case
printf dirty >"$GATE_ROOT/untracked"
assert_result 1 'dirty gate root is rejected before credential use' run_writer
assert_no_patch 'dirty gate root does not PATCH'
rm -f "$GATE_ROOT/untracked"

new_case
git -C "$GATE_ROOT" checkout -q main
assert_result 1 'attached gate HEAD is rejected before credential use' run_writer
assert_no_patch 'attached gate root does not PATCH'
git -C "$GATE_ROOT" checkout -q --detach "$G"

new_case
git -C "$GATE_ROOT" checkout -q --detach "$SIDE"
assert_result 1 'wrong detached gate HEAD is rejected before credential use' run_writer
assert_no_patch 'wrong detached gate HEAD does not PATCH'
git -C "$GATE_ROOT" checkout -q --detach "$G"

new_case
git -C "$GATE_ROOT" config core.repositoryFormatVersion 1
git -C "$GATE_ROOT" config extensions.partialClone origin
assert_result 1 'partial-clone extension is rejected before credential use' run_writer
assert_no_patch 'partial-clone extension does not PATCH'
git -C "$GATE_ROOT" config --unset extensions.partialClone
git -C "$GATE_ROOT" config core.repositoryFormatVersion 0

new_case
git -C "$GATE_ROOT" config remote.origin.promisor true
assert_result 1 'promisor remote configuration is rejected before credential use' run_writer
assert_no_patch 'promisor remote configuration does not PATCH'
git -C "$GATE_ROOT" config --unset remote.origin.promisor

new_case
ALTERNATE_OBJECTS="$CASE_ROOT/alternate-objects"
mkdir -p "$ALTERNATE_OBJECTS"
GATE_GIT_DIR=$(git -C "$GATE_ROOT" rev-parse --absolute-git-dir)
mkdir -p "$GATE_GIT_DIR/objects/info"
printf '%s\n' "$ALTERNATE_OBJECTS" >"$GATE_GIT_DIR/objects/info/alternates"
assert_result 1 'on-disk object alternates are rejected before credential use' run_writer
assert_no_patch 'on-disk object alternates do not PATCH'
rm -f "$GATE_GIT_DIR/objects/info/alternates"

new_case
ALTERNATE_OBJECTS="$CASE_ROOT/environment-alternate-objects"
mkdir -p "$ALTERNATE_OBJECTS"
assert_result 1 'environment-provided object alternates are rejected' \
  run_writer_with_git_alternate "$ALTERNATE_OBJECTS"
assert_no_patch 'environment-provided object alternates do not PATCH'

new_case
touch "$FAKE_BIN/user.transport-failure"
assert_result 1 'transport errors are sanitized and temporary files are cleaned' run_writer
residue=$(find /tmp -maxdepth 1 -name '.edgezero-publisher-*' -print -quit)
if [[ -z "$residue" ]]; then ok 'temporary files are removed on failure'; else no 'temporary files are removed on failure'; fi

new_case
touch "$FAKE_BIN/user.block"
PATH="$FAKE_BIN:$PATH" \
  EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN="$TOKEN" \
  AMBIENT_SECRET=must-not-reach-tools \
  /bin/bash "$WRITE" \
    --gate-root "$CLI_GATE_ROOT" --gate-sha "$CLI_GATE_SHA" \
    --evidence-json "$EVIDENCE_JSON" \
    --publisher-prerequisite-json "$PREREQUISITE_JSON" \
    --writer-token-review-json "$REVIEW_JSON" \
    --writer-token-review-png "$PNG" \
    >"$CASE_ROOT/signal-stdout" 2>"$CASE_ROOT/signal-stderr" &
writer_pid=$!
deadline=$((SECONDS + 3))
while [[ ! -f "$FAKE_BIN/curl-blocked" && $SECONDS -lt $deadline ]]; do sleep 0.01; done
signal_status=0
if [[ -f "$FAKE_BIN/curl-blocked" ]]; then
  kill -TERM "$writer_pid" 2>/dev/null || true
  kill -TERM "$(<"$FAKE_BIN/curl-child-pid")" 2>/dev/null || true
else
  : >"$FAKE_BIN/release-curl"
fi
wait "$writer_pid" 2>/dev/null || signal_status=$?
residue=$(find /tmp -maxdepth 1 -name '.edgezero-publisher-*' -print -quit)
if [[ "$signal_status" -eq 143 && ! -s "$CASE_ROOT/signal-stdout" && -z "$residue" ]]; then
  ok 'TERM preserves status 143 and cleans all temporary files'
else
  cat "$CASE_ROOT/signal-stdout" "$CASE_ROOT/signal-stderr" >&2
  no 'TERM preserves status 143 and cleans all temporary files'
fi
assert_no_patch 'signal failure does not PATCH'

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
}

cat >>"$FAKE_BIN/curl" <<'SH'
[[ -n "$request" && -n "$output" && -n "$url" ]]
case "$request $url" in
  "GET https://api.github.com/user") endpoint=user ;;
  "GET https://api.github.com/orgs/stackpop/memberships/"*) endpoint=membership ;;
  "GET https://api.github.com/repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_GATE_SHA") endpoint=gate ;;
  "GET https://api.github.com/repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE")
    reads=0
    [[ ! -f "$fixture/prerequisite-reads" ]] || reads=$(<"$fixture/prerequisite-reads")
    reads=$((reads + 1))
    printf '%s' "$reads" >"$fixture/prerequisite-reads"
    if [[ "$reads" -eq 1 ]]; then endpoint=before; else endpoint=after; fi
    ;;
  "PATCH https://api.github.com/repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE")
    endpoint=patch
    [[ "$data" == @* ]]
    cat "${data#@}" >"$fixture/patch-body"
    ;;
  *) printf 'unexpected request\n' >&2; exit 97 ;;
esac
if [[ -f "$fixture/$endpoint.transport-failure" ]]; then
  printf 'transport detail must not leak\n' >&2
  exit 28
fi
if [[ -f "$fixture/$endpoint.block" ]]; then
  printf '%s' "$$" >"$fixture/curl-child-pid"
  : >"$fixture/curl-blocked"
  trap 'exit 143' TERM
  while [[ ! -f "$fixture/release-curl" ]]; do sleep 0.01; done
fi
cat "$fixture/$endpoint.body" >"$output"
printf '%s\n%s\n%s' \
  "$(<"$fixture/$endpoint.status")" \
  "$(<"$fixture/$endpoint.version")" \
  "$(<"$fixture/$endpoint.media")"
SH
chmod 0755 "$FAKE_BIN/curl"

bootstrap_record() {
  local evidence_url=null source_pr=null
  if [[ "$4" != null ]]; then
    source_pr="\"${5:-$SOURCE_PR}\""
    evidence_url="\"${6:-https://github.com/stackpop/edgezero/pull/${5:-$SOURCE_PR}#issuecomment-$EVIDENCE_COMMENT_ID}\""
  fi
  printf '{"evidence-sha256":"%s","evidence-url":%s,"gate-sha":"%s","previous-value-sha256":%s,"rotation-history":{"state":"bootstrap-no-rotation"},"schema-version":2,"source-pr":%s,"source-revision":%s}' \
    "$1" "$evidence_url" "$2" "$3" "$source_pr" "$4"
}

verified_record() {
  local created=${7:-$ROTATION_AT} nested=${8:-$ROTATION_DIGEST}
  local run_number=${9:-$5} history=${10:-$HISTORY_DIGEST}
  local evidence_url=null source_pr=null
  if [[ "$4" != null ]]; then
    source_pr="\"${11:-$SOURCE_PR}\""
    evidence_url="\"${12:-https://github.com/stackpop/edgezero/pull/${11:-$SOURCE_PR}#issuecomment-$EVIDENCE_COMMENT_ID}\""
  fi
  printf '{"evidence-sha256":"%s","evidence-url":%s,"gate-sha":"%s","previous-value-sha256":%s,"rotation-history":{"created-at":"%s","evidence-sha256":"%s","history-sha256":"%s","run-attempt":"%s","run-id":"%s","run-number":"%s","state":"verified"},"schema-version":2,"source-pr":%s,"source-revision":%s}' \
    "$1" "$evidence_url" "$2" "$3" "$created" "$nested" "$history" "$6" "$5" "$run_number" "$source_pr" "$4"
}

review_json() {
  local reviewed_at=${1:-$REVIEWED_AT} expires_at=${2:-$EXPIRES_AT}
  local reviewer=${3:-security-reviewer} subject=${4:-variable-writer}
  printf '{"expires-at":"%s","organization-grants":{"members":"read","other-displayed":"none"},"repository-grants":{"metadata":"read","other-displayed":"none","variables":"write"},"resource-owner":"stackpop","reviewed-at":"%s","reviewer-login":"%s","schema-version":1,"screenshot-sha256":"%s","selected-repositories":["stackpop/edgezero"],"subject-login":"%s","token-id":"%s"}' \
    "$expires_at" "$reviewed_at" "$reviewer" "$PNG_DIGEST" "$subject" "$TOKEN_ID"
}

set_metadata() {
  local endpoint=$1 status=${2:-200} version=${3:-2026-03-10} media=${4:-application/json}
  printf '%s' "$status" >"$FAKE_BIN/$endpoint.status"
  printf '%s' "$version" >"$FAKE_BIN/$endpoint.version"
  printf '%s' "$media" >"$FAKE_BIN/$endpoint.media"
}

write_variable_body() {
  jq -cn --arg name "$VARIABLE" --arg value "$2" \
    '{name:$name,value:$value,created_at:"2026-09-10T10:00:00Z",updated_at:"2026-09-10T11:00:00Z"}' >"$1"
}

write_variable_body_control() {
  local suffix
  case "$3" in
    lf) suffix='\n' ;;
    nul) suffix='\u0000' ;;
    *) return 2 ;;
  esac
  jq -cn --arg name "$VARIABLE" --arg value "$2" --argjson suffix "\"$suffix\"" \
    '{name:$name,value:($value + $suffix),created_at:"2026-09-10T10:00:00Z",updated_at:"2026-09-10T11:00:00Z"}' >"$1"
}

set_current() {
  CURRENT=$1
  write_variable_body "$FAKE_BIN/before.body" "$CURRENT"
}

set_requested() {
  REQUESTED=$1
  printf '%s' "$REQUESTED" >"$PREREQUISITE_JSON"
  write_variable_body "$FAKE_BIN/after.body" "$REQUESTED"
}

reset_api() {
  rm -f -- "$FAKE_BIN"/args-* "$FAKE_BIN"/config-* "$FAKE_BIN/curl-count" \
    "$FAKE_BIN/prerequisite-reads" "$FAKE_BIN/patch-body" "$FAKE_BIN"/*.transport-failure \
    "$FAKE_BIN"/*.block "$FAKE_BIN/curl-child-pid" "$FAKE_BIN/curl-blocked" \
    "$FAKE_BIN/release-curl"
  jq -cn --arg login variable-writer --argjson id "$TOKEN_ID" '{login:$login,id:$id}' >"$FAKE_BIN/user.body"
  jq -cn '{state:"active",user:{login:"variable-writer"}}' >"$FAKE_BIN/membership.body"
  jq -cn --arg value "$G" \
    '{name:"EDGEZERO_BUILD_CONTAINER_GATE_SHA",value:$value,created_at:"2026-09-10T10:00:00Z",updated_at:"2026-09-10T11:00:00Z"}' >"$FAKE_BIN/gate.body"
  : >"$FAKE_BIN/patch.body"
  for endpoint in user membership gate before after; do set_metadata "$endpoint"; done
  set_metadata patch 204 2026-03-10 ''
}

new_case() {
  case_number=$((case_number + 1))
  CASE_ROOT="$INPUT_ROOT/case-$case_number"
  mkdir "$CASE_ROOT"
  EVIDENCE_JSON="$CASE_ROOT/evidence.json"
  PREREQUISITE_JSON="$CASE_ROOT/prerequisite.json"
  REVIEW_JSON="$CASE_ROOT/review.json"
  PNG="$CASE_ROOT/review.png"
  printf 'opaque-audit-evidence-%s' "$case_number" >"$EVIDENCE_JSON"
  printf '\211PNG\r\n\032\nfixture-png-%s' "$case_number" >"$PNG"
  EVIDENCE_DIGEST="sha256:$(hash_file "$EVIDENCE_JSON")"
  PNG_DIGEST="sha256:$(hash_file "$PNG")"
  review_json >"$REVIEW_JSON"
  reset_api
  current=$(bootstrap_record "sha256:$(printf '1%.0s' {1..64})" "$G" null null)
  set_current "$current"
  previous="\"sha256:$(hash_bytes "$current")\""
  set_requested "$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S1\"")"
  CLI_GATE_ROOT=$GATE_ROOT
  CLI_GATE_SHA=$G
}

run_writer() {
  PATH="$FAKE_BIN:$PATH" \
    EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN="$TOKEN" \
    AMBIENT_SECRET=must-not-reach-tools \
    bash "$WRITE" \
      --gate-root "$CLI_GATE_ROOT" --gate-sha "$CLI_GATE_SHA" \
      --evidence-json "$EVIDENCE_JSON" \
      --publisher-prerequisite-json "$PREREQUISITE_JSON" \
      --writer-token-review-json "$REVIEW_JSON" \
      --writer-token-review-png "$PNG" "$@"
}

run_writer_with_path() {
  local prefix=$1
  PATH="$prefix:$FAKE_BIN:$PATH" \
    EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN="$TOKEN" \
    AMBIENT_SECRET=must-not-reach-tools \
    bash "$WRITE" \
      --gate-root "$CLI_GATE_ROOT" --gate-sha "$CLI_GATE_SHA" \
      --evidence-json "$EVIDENCE_JSON" \
      --publisher-prerequisite-json "$PREREQUISITE_JSON" \
      --writer-token-review-json "$REVIEW_JSON" \
      --writer-token-review-png "$PNG"
}

run_writer_with_shellopts() {
  env SHELLOPTS=xtrace:allexport \
    PATH="$FAKE_BIN:$PATH" \
    EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN="$TOKEN" \
    TOKEN=ambient-exported-token-alias \
    WRITER_TOKEN=ambient-exported-writer-token-alias \
    AMBIENT_SECRET=must-not-reach-tools \
    bash "$WRITE" \
      --gate-root "$CLI_GATE_ROOT" --gate-sha "$CLI_GATE_SHA" \
      --evidence-json "$EVIDENCE_JSON" \
      --publisher-prerequisite-json "$PREREQUISITE_JSON" \
      --writer-token-review-json "$REVIEW_JSON" \
      --writer-token-review-png "$PNG"
}

run_writer_with_git_alternate() {
  local alternate=$1
  GIT_ALTERNATE_OBJECT_DIRECTORIES="$alternate" \
    PATH="$FAKE_BIN:$PATH" \
    EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN="$TOKEN" \
    AMBIENT_SECRET=must-not-reach-tools \
    bash "$WRITE" \
      --gate-root "$CLI_GATE_ROOT" --gate-sha "$CLI_GATE_SHA" \
      --evidence-json "$EVIDENCE_JSON" \
      --publisher-prerequisite-json "$PREREQUISITE_JSON" \
      --writer-token-review-json "$REVIEW_JSON" \
      --writer-token-review-png "$PNG"
}

curl_calls() {
  if [[ -f "$FAKE_BIN/curl-count" ]]; then cat "$FAKE_BIN/curl-count"; else printf 0; fi
}

patch_calls() {
  local count=0 path
  for path in "$FAKE_BIN"/args-*; do
    [[ -f "$path" ]] || continue
    if grep -Fxq PATCH "$path"; then count=$((count + 1)); fi
  done
  printf '%s' "$count"
}

assert_result() {
  local expected=$1 description=$2 status=0
  shift 2
  "$@" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$?
  if [[ "$status" -ne "$expected" ]]; then
    cat "$CASE_ROOT/stdout" "$CASE_ROOT/stderr" >&2
    no "$description (status $status, expected $expected)"
  elif [[ -s "$CASE_ROOT/stdout" ]]; then
    no "$description emitted stdout"
  elif [[ "$expected" -eq 0 && -s "$CASE_ROOT/stderr" ]]; then
    cat "$CASE_ROOT/stderr" >&2
    no "$description emitted stderr"
  elif [[ "$expected" -ne 0 && (! -s "$CASE_ROOT/stderr" || $(<"$CASE_ROOT/stderr") == *"$TOKEN"*) ]]; then
    cat "$CASE_ROOT/stderr" >&2
    no "$description did not emit sanitized diagnostics"
  else
    ok "$description"
  fi
}

assert_no_patch() {
  if [[ $(patch_calls) == 0 ]]; then ok "$1"; else no "$1"; fi
}

configure_transition() {
  set_current "$1"
  set_requested "$2"
}

assert_transport_contract() {
  local good=true call args config output data url index expected_body
  local -a actual expected
  [[ $(curl_calls) == 6 ]] || good=false
  for call in 1 2 3 4 5 6; do
    args="$FAKE_BIN/args-$call"
    config="$FAKE_BIN/config-$call"
    [[ -f "$args" && -f "$config" ]] || { good=false; continue; }
    actual=()
    while IFS= read -r argument || [[ -n "$argument" ]]; do
      actual+=("$argument")
    done <"$args"
    if [[ "$call" -eq 5 ]]; then
      data=${actual[14]:-}
      output=${actual[16]:-}
      expected=(
        --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0
        --request PATCH --config - --data-binary "$data" --output "$output"
        --write-out '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}'
        "$API/repos/stackpop/edgezero/actions/variables/$VARIABLE"
      )
      [[ "$data" == @/tmp/.edgezero-publisher-request.?????? ]] || good=false
      printf '%s\n' \
        'header = "Accept: application/vnd.github+json"' \
        'header = "X-GitHub-Api-Version: 2026-03-10"' \
        'header = "User-Agent: edgezero-build-container-gate/1"' \
        'header = "Content-Type: application/json"' \
        "header = \"Authorization: Bearer $TOKEN\"" >"$CASE_ROOT/expected-config"
    else
      output=${actual[14]:-}
      case "$call" in
        1) url="$API/user" ;;
        2) url="$API/orgs/stackpop/memberships/variable-writer" ;;
        3) url="$API/repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_GATE_SHA" ;;
        4 | 6) url="$API/repos/stackpop/edgezero/actions/variables/$VARIABLE" ;;
      esac
      expected=(
        --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0
        --request GET --config - --output "$output"
        --write-out '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}' "$url"
      )
      printf '%s\n' \
        'header = "Accept: application/vnd.github+json"' \
        'header = "X-GitHub-Api-Version: 2026-03-10"' \
        'header = "User-Agent: edgezero-build-container-gate/1"' \
        "header = \"Authorization: Bearer $TOKEN\"" >"$CASE_ROOT/expected-config"
    fi
    cmp -s "$CASE_ROOT/expected-config" "$config" || good=false
    [[ "${#actual[@]}" -eq "${#expected[@]}" ]] || good=false
    for index in "${!expected[@]}"; do
      [[ "${actual[index]:-}" == "${expected[index]}" ]] || good=false
    done
  done
  expected_body=$(jq -cn --arg name "$VARIABLE" --arg value "$REQUESTED" '{name:$name,value:$value}')
  printf '%s' "$expected_body" >"$CASE_ROOT/expected-body"
  [[ -f "$FAKE_BIN/patch-body" ]] && cmp -s "$CASE_ROOT/expected-body" "$FAKE_BIN/patch-body" || good=false
  if [[ "$good" == true ]]; then
    ok 'curl uses exact ordered routes, flags, headers, API version, and PATCH body'
  else
    no 'curl uses exact ordered routes, flags, headers, API version, and PATCH body'
  fi
}

echo '== publisher prerequisite writer =='

new_case
assert_result 0 'inert state can bind the first source silently' run_writer
assert_transport_contract

for boundary in minimum maximum; do
  new_case
  if [[ "$boundary" == minimum ]]; then
    printf x >"$EVIDENCE_JSON"
    printf '\211PNG\r\n\032\n' >"$PNG"
  else
    dd if=/dev/zero of="$EVIDENCE_JSON" bs=1048576 count=1 2>/dev/null
    printf '\211PNG\r\n\032\n' >"$PNG"
    dd if=/dev/zero bs=10485752 count=1 2>/dev/null >>"$PNG"
  fi
  EVIDENCE_DIGEST="sha256:$(hash_file "$EVIDENCE_JSON")"
  PNG_DIGEST="sha256:$(hash_file "$PNG")"
  review_json >"$REVIEW_JSON"
  previous="\"sha256:$(hash_bytes "$CURRENT")\""
  set_requested "$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S1\"")"
  assert_result 0 "exact $boundary evidence and PNG bounds are accepted" run_writer
done

new_case
current=$(bootstrap_record "sha256:$(printf '1%.0s' {1..64})" "$G" null null)
set_current "$current"
previous="\"sha256:$(hash_bytes "$current")\""
set_requested "$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null \
  18446744073709551615 4294967295 "$ROTATION_AT" "$ROTATION_DIGEST" \
  18446744073709551615)"
assert_result 0 'maximum run attempt, id, and number bounds are accepted' run_writer

new_case
current=$(bootstrap_record "sha256:$(printf '1%.0s' {1..64})" "$G" null null)
set_current "$current"
previous="\"sha256:$(hash_bytes "$current")\""
set_requested "$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S1\"" \
  18446744073709551615 \
  https://github.com/stackpop/edgezero/pull/18446744073709551615#issuecomment-18446744073709551615)"
assert_result 0 'maximum source PR and evidence comment id bounds are accepted' run_writer

new_case
current=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" null null)
set_current "$current"
set_requested "$current"
assert_result 0 'byte-identical state is an authenticated idempotent success' run_writer
if [[ $(curl_calls) == 4 && $(patch_calls) == 0 ]]; then
  ok 'idempotence reads current state but skips PATCH and readback'
else
  no 'idempotence reads current state but skips PATCH and readback'
fi

for transition in same-source-refresh same-source-url-refresh forward-source forward-source-tuple \
  verified-forward-source gate-rotation \
  verified-gate-rotation first-bootstrap-rollback clear-on-forward-rotation inert-forward-rotation; do
  new_case
  old_previous="\"sha256:$(printf '3%.0s' {1..64})\""
  case "$transition" in
    same-source-refresh)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"")
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S1\"")
      ;;
    same-source-url-refresh)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"" \
        347 https://github.com/stackpop/edgezero/pull/347#issuecomment-700)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S1\"" \
        347 https://github.com/stackpop/edgezero/pull/347#issuecomment-701)
      ;;
    forward-source)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"")
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S2\"")
      ;;
    forward-source-tuple)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"" \
        347 https://github.com/stackpop/edgezero/pull/347#issuecomment-700)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S2\"" \
        348 https://github.com/stackpop/edgezero/pull/348#issuecomment-701)
      ;;
    verified-forward-source)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"" 10 1)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S2\"" 10 1)
      ;;
    gate-rotation)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$OLD_G" "$old_previous" null)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 9007199254740993 2)
      ;;
    verified-gate-rotation)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$OLD_G" "$old_previous" null 10 1)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 11 1 \
        2026-09-10T11:01:00Z "sha256:$(printf '9%.0s' {1..64})" 11 "sha256:$(printf '6%.0s' {1..64})")
      ;;
    first-bootstrap-rollback)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"")
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 1)
      ;;
    clear-on-forward-rotation)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"" 10 1)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 11 1 \
        2026-09-10T11:01:00Z "sha256:$(printf '9%.0s' {1..64})" 11 "sha256:$(printf '6%.0s' {1..64})")
      ;;
    inert-forward-rotation)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" null 10 1)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 11 1 \
        2026-09-10T11:01:00Z "sha256:$(printf '9%.0s' {1..64})" 11 "sha256:$(printf '6%.0s' {1..64})")
      ;;
  esac
  configure_transition "$from" "$to"
  assert_result 0 "$transition transition is accepted" run_writer
done

for transition in null-current wrong-previous source-clear source-regression source-incomparable \
  source-missing source-before-gate cross-gate-source gate-bootstrap rotation-regression \
  gate-stale-rotation rotation-same-identity-changed rotation-forward-stale-detail \
  rotation-same-run-id rotation-stale-receipt rotation-stale-history \
  source-history-change same-source-pr-change verified-to-bootstrap; do
  new_case
  old_previous="\"sha256:$(printf '3%.0s' {1..64})\""
  from=$CURRENT
  previous="\"sha256:$(hash_bytes "$from")\""
  case "$transition" in
    null-current)
      jq -cn --arg name "$VARIABLE" '{name:$name,value:null}' >"$FAKE_BIN/before.body"
      to=$REQUESTED
      ;;
    wrong-previous)
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" '"sha256:'"$(printf 'f%.0s' {1..64})"'"' "\"$S1\"")
      ;;
    source-clear)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"" 10 1)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 1)
      ;;
    source-regression)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S2\"")
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S1\"")
      ;;
    source-incomparable)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"")
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$SIDE\"")
      ;;
    source-missing)
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$(printf 'f%.0s' {1..40})\"")
      ;;
    source-before-gate)
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$OLD_G\"")
      ;;
    cross-gate-source)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$OLD_G" "$old_previous" null)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S1\"" 7 1)
      ;;
    gate-bootstrap)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$OLD_G" "$old_previous" null)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" null)
      ;;
    rotation-regression)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" null 10 2)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 9 1 \
        2026-09-10T11:01:00Z "sha256:$(printf '9%.0s' {1..64})" 9 "sha256:$(printf '6%.0s' {1..64})")
      ;;
    gate-stale-rotation)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$OLD_G" "$old_previous" null 10 2)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 2)
      ;;
    rotation-same-identity-changed)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" null 10 2)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 2 2026-09-10T11:01:00Z)
      ;;
    rotation-forward-stale-detail)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" null 10 2)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 11 1 "$ROTATION_AT" "$ROTATION_DIGEST" 11 "$HISTORY_DIGEST")
      ;;
    rotation-same-run-id)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" null 10 1)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 10 2 \
        2026-09-10T11:01:00Z "sha256:$(printf '9%.0s' {1..64})" 11 "sha256:$(printf '6%.0s' {1..64})")
      ;;
    rotation-stale-receipt)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" null 10 1)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 11 1 \
        2026-09-10T11:01:00Z "$ROTATION_DIGEST" 11 "sha256:$(printf '6%.0s' {1..64})")
      ;;
    rotation-stale-history)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" null 10 1)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" null 11 1 \
        2026-09-10T11:01:00Z "sha256:$(printf '9%.0s' {1..64})" 11 "$HISTORY_DIGEST")
      ;;
    source-history-change)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"" 10 1)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(verified_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S2\"" 11 1 \
        2026-09-10T11:01:00Z "sha256:$(printf '9%.0s' {1..64})" 11 "sha256:$(printf '6%.0s' {1..64})")
      ;;
    same-source-pr-change)
      from=$(bootstrap_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" "\"$S1\"" \
        347 https://github.com/stackpop/edgezero/pull/347#issuecomment-700)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" "\"$S1\"" \
        348 https://github.com/stackpop/edgezero/pull/348#issuecomment-701)
      ;;
    verified-to-bootstrap)
      from=$(verified_record "sha256:$(printf '2%.0s' {1..64})" "$G" "$old_previous" null 10 2)
      previous="\"sha256:$(hash_bytes "$from")\""
      to=$(bootstrap_record "$EVIDENCE_DIGEST" "$G" "$previous" null)
      ;;
  esac
  if [[ "$transition" != null-current ]]; then configure_transition "$from" "$to"; else set_requested "$to"; fi
  assert_result 1 "$transition transition is rejected" run_writer
  assert_no_patch "$transition fails before PATCH"
done

run_remaining_tests
