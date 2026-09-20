#!/usr/bin/env bash
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
TOOLS="$DIR/../../../docker/build-app-cli"
WRITE="$TOOLS/write-image-release-record.sh"
CHECK="$TOOLS/check-image-pin.sh"
WORK_RAW=$(mktemp -d)
WORK=$(cd -- "$WORK_RAW" && pwd -P)
trap 'rm -rf "$WORK"' EXIT

REPO="ghcr.io/stackpop/edgezero-build-app-cli"
DIGEST="sha256:$(printf '1%.0s' {1..64})"
SOURCE=$(printf '2%.0s' {1..40})
CHALLENGE=$(printf '3%.0s' {1..64})
SCREENSHOT="sha256:$(printf '4%.0s' {1..64})"
TAG="build-container-v7"
NOW=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
RUN_ID="9007199254740993"
RUN_ATTEMPT="2"

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
assert_pass() {
  local description=$1
  shift
  if "$@" >/dev/null 2>&1; then ok "$description"; else no "$description"; fi
}
assert_fail() {
  local description=$1
  shift
  if "$@" >/dev/null 2>&1; then no "$description"; else ok "$description"; fi
}
assert_bytes() {
  local description=$1 expected=$2 path=$3 expected_file="$WORK/expected"
  printf '%s' "$expected" >"$expected_file"
  if cmp -s "$expected_file" "$path"; then ok "$description"; else no "$description"; fi
}
assert_silent_success() {
  local description=$1 image_path=$2 evidence_path=$3 stdout_file="$WORK/stdout" stderr_file="$WORK/stderr"
  if run_writer "$image_path" "$evidence_path" >"$stdout_file" 2>"$stderr_file" &&
    [[ ! -s "$stdout_file" && ! -s "$stderr_file" ]]; then
    ok "$description"
  else
    no "$description"
  fi
}

writer_args() {
  printf '%s\0' \
    --repository "$REPO" \
    --release-tag "$TAG" \
    --image-digest "$DIGEST" \
    --source-revision "$SOURCE" \
    --provenance-protocol 1 \
    --approval-challenge "$CHALLENGE" \
    --approver-login release-reviewer \
    --reviewed-at "$NOW" \
    --run-attempt "$RUN_ATTEMPT" \
    --run-id "$RUN_ID" \
    --screenshot-sha256 "$SCREENSHOT"
}

run_writer() {
  local image_path=$1 evidence_path=$2
  shift 2
  local -a args=()
  while IFS= read -r -d '' arg; do args+=("$arg"); done < <(writer_args)
  bash "$WRITE" --image-path "$image_path" --evidence-path "$evidence_path" "${args[@]}" "$@"
}

write_image() {
  local path=$1 digest=${2:-$DIGEST} source=${3:-$SOURCE} tag=${4:-$TAG}
  printf '%s' "{\"digest\":\"$digest\",\"image-source-revision\":\"$source\",\"provenance-protocol\":1,\"repository\":\"$REPO\",\"tag\":\"$tag\"}" >"$path"
}

write_evidence() {
  local path=$1 content
  if [[ $# -eq 2 ]]; then
    content=$2
  else
    content="{\"approval-challenge\":\"$CHALLENGE\",\"approver-login\":\"release-reviewer\",\"image-digest\":\"$DIGEST\",\"release-tag\":\"$TAG\",\"reviewed-at\":\"$NOW\",\"run-attempt\":\"$RUN_ATTEMPT\",\"run-id\":\"$RUN_ID\",\"schema-version\":1,\"screenshot-sha256\":\"$SCREENSHOT\",\"source-revision\":\"$SOURCE\"}"
  fi
  printf '%s' "$content" >"$path"
}

echo "== build container image release record writer and pair validator =="

image="$WORK/image.json"
evidence="$WORK/image-release-evidence.json"
assert_pass "typed writer creates both records" run_writer "$image" "$evidence"
expected_image="{\"digest\":\"$DIGEST\",\"image-source-revision\":\"$SOURCE\",\"provenance-protocol\":1,\"repository\":\"$REPO\",\"tag\":\"$TAG\"}"
expected_evidence="{\"approval-challenge\":\"$CHALLENGE\",\"approver-login\":\"release-reviewer\",\"image-digest\":\"$DIGEST\",\"release-tag\":\"$TAG\",\"reviewed-at\":\"$NOW\",\"run-attempt\":\"$RUN_ATTEMPT\",\"run-id\":\"$RUN_ID\",\"schema-version\":1,\"screenshot-sha256\":\"$SCREENSHOT\",\"source-revision\":\"$SOURCE\"}"
assert_bytes "image record bytes are exact and newline-free" "$expected_image" "$image"
assert_bytes "evidence bytes are exact JCS and newline-free" "$expected_evidence" "$evidence"
assert_pass "writer output passes the independent pair validator" \
  bash "$CHECK" validate-pair "$image" "$evidence"
assert_fail "writer never replaces an existing pair" run_writer "$image" "$evidence"

mkdir "$WORK/preflight"
printf sentinel >"$WORK/preflight/evidence.json"
assert_fail "an existing evidence path blocks before image creation" \
  run_writer "$WORK/preflight/image.json" "$WORK/preflight/evidence.json"
if [[ ! -e "$WORK/preflight/image.json" ]]; then
  ok "blocked preflight leaves no partial image record"
else
  no "blocked preflight leaves no partial image record"
fi

mkdir "$WORK/preflight-image"
printf sentinel >"$WORK/preflight-image/image.json"
assert_fail "an existing image path blocks before evidence creation" \
  run_writer "$WORK/preflight-image/image.json" "$WORK/preflight-image/evidence.json"
if [[ ! -e "$WORK/preflight-image/evidence.json" ]]; then
  ok "blocked image preflight leaves no partial evidence record"
else
  no "blocked image preflight leaves no partial evidence record"
fi

assert_fail "an unknown writer flag is rejected" \
  run_writer "$WORK/unknown-image.json" "$WORK/unknown-evidence.json" --raw-json '{}'
assert_fail "a duplicate writer flag is rejected" \
  run_writer "$WORK/dup-image.json" "$WORK/dup-evidence.json" --run-id 7
assert_fail "a normalized protocol spelling is rejected" \
  run_writer "$WORK/protocol-image.json" "$WORK/protocol-evidence.json" \
  --provenance-protocol 1.0

mkdir "$WORK/other-parent"
assert_fail "the writer cannot split the record pair across directories" \
  run_writer "$WORK/split-image.json" "$WORK/other-parent/split-evidence.json"

mkdir "$WORK/silent"
chmod 0700 "$WORK/silent"
assert_silent_success "writer success is silent" \
  "$WORK/silent/image.json" "$WORK/silent/evidence.json"

mkdir "$WORK/public-parent"
chmod 0755 "$WORK/public-parent"
assert_fail "writer rejects a non-private output parent" \
  run_writer "$WORK/public-parent/image.json" "$WORK/public-parent/evidence.json"

mkdir -p "$WORK/nested/child"
chmod 0700 "$WORK/nested" "$WORK/nested/child"
assert_fail "writer outputs must be direct children of the canonical parent path" \
  run_writer "$WORK/nested/child/../image.json" "$WORK/nested/evidence.json"

mkdir "$WORK/repository"
git -C "$WORK/repository" init -q
mkdir "$WORK/repository/private"
chmod 0700 "$WORK/repository/private"
assert_fail "writer rejects an output parent inside a Git repository" \
  run_writer "$WORK/repository/private/image.json" "$WORK/repository/private/evidence.json"

mkdir "$WORK/relative"
chmod 0700 "$WORK/relative"
# The child shell expands its positional parameters (intentional SC2016).
# shellcheck disable=SC2016
assert_fail "writer rejects relative output paths" \
  bash -c 'cd "$1" && shift && "$@"' _ "$WORK" \
  bash "$WRITE" --image-path relative/image.json --evidence-path relative/evidence.json \
  --repository "$REPO" --release-tag "$TAG" --image-digest "$DIGEST" \
  --source-revision "$SOURCE" --provenance-protocol 1 --approval-challenge "$CHALLENGE" \
  --approver-login release-reviewer --reviewed-at "$NOW" --run-attempt "$RUN_ATTEMPT" \
  --run-id "$RUN_ID" --screenshot-sha256 "$SCREENSHOT"

args=()
while IFS= read -r -d '' arg; do args+=("$arg"); done < <(writer_args)
missing_args=()
skip_next=false
for arg in "${args[@]}"; do
  if [[ "$skip_next" == true ]]; then
    skip_next=false
    continue
  fi
  if [[ "$arg" == --run-attempt ]]; then
    skip_next=true
    continue
  fi
  missing_args+=("$arg")
done
assert_fail "a missing typed scalar is rejected" \
  bash "$WRITE" --image-path "$WORK/missing-image.json" \
  --evidence-path "$WORK/missing-evidence.json" "${missing_args[@]}"

bad_image="$WORK/manual-image.json"
bad_evidence="$WORK/manual-evidence.json"
write_image "$bad_image"
write_evidence "$bad_evidence"

printf '%s' "${expected_evidence/\"run-id\":\"$RUN_ID\"/\"run-id\":\"18446744073709551616\"}" >"$bad_evidence"
assert_fail "u64 overflow run id is rejected" bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
printf '%s' "${expected_evidence/\"run-attempt\":\"$RUN_ATTEMPT\"/\"run-attempt\":\"4294967296\"}" >"$bad_evidence"
assert_fail "u32 overflow run attempt is rejected" bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
printf '%s' "${expected_evidence/\"run-attempt\":\"$RUN_ATTEMPT\"/\"run-attempt\":\"02\"}" >"$bad_evidence"
assert_fail "noncanonical run attempt is rejected" bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"

for replacement in \
  "\"schema-version\":\"1\"" \
  "\"schema-version\":1.5" \
  "\"schema-version\":2"; do
  printf '%s' "${expected_evidence/\"schema-version\":1/$replacement}" >"$bad_evidence"
  assert_fail "invalid $replacement is rejected" bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
done

max_evidence="${expected_evidence/\"run-attempt\":\"$RUN_ATTEMPT\"/\"run-attempt\":\"4294967295\"}"
max_evidence="${max_evidence/\"run-id\":\"$RUN_ID\"/\"run-id\":\"18446744073709551615\"}"
printf '%s' "$max_evidence" >"$bad_evidence"
assert_pass "maximum u32/u64 run identifiers retain exact precision" \
  bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"

for field in approval-challenge approver-login image-digest release-tag reviewed-at run-attempt run-id \
  screenshot-sha256 source-revision; do
  invalid=$(jq -cS --arg field "$field" '.[$field]=7' "$evidence")
  printf '%s' "$invalid" >"$bad_evidence"
  assert_fail "non-string evidence $field is rejected" \
    bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
done

for mutation in \
  "${expected_evidence/\"image-digest\":\"$DIGEST\"/\"image-digest\":\"sha256:$(printf '5%.0s' {1..64})\"}" \
  "${expected_evidence/\"release-tag\":\"$TAG\"/\"release-tag\":\"build-container-v8\"}" \
  "${expected_evidence/\"source-revision\":\"$SOURCE\"/\"source-revision\":\"$(printf '6%.0s' {1..40})\"}"; do
  printf '%s' "$mutation" >"$bad_evidence"
  assert_fail "a cross-file mismatch is rejected" bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
done

printf '%s\n' "$expected_evidence" >"$bad_evidence"
assert_fail "a trailing newline violates canonical evidence bytes" \
  bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
printf ' %s' "$expected_evidence" >"$bad_evidence"
assert_fail "surrounding whitespace violates canonical evidence bytes" \
  bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
reordered=$(jq -c '{"approver-login":."approver-login",
  "approval-challenge":."approval-challenge","image-digest":."image-digest",
  "release-tag":."release-tag","reviewed-at":."reviewed-at","run-attempt":."run-attempt",
  "run-id":."run-id","schema-version":."schema-version",
  "screenshot-sha256":."screenshot-sha256","source-revision":."source-revision"}' "$evidence")
printf '%s' "$reordered" >"$bad_evidence"
assert_fail "reordered evidence keys are rejected" bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
printf '%s' "${expected_evidence/\"approval-challenge\":\"$CHALLENGE\"/\"approval-challenge\":\"$CHALLENGE\",\"unexpected\":true}" >"$bad_evidence"
assert_fail "an extra evidence key is rejected" bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
printf '%s' "${expected_evidence/\"approval-challenge\":\"$CHALLENGE\",/}" >"$bad_evidence"
assert_fail "a missing evidence key is rejected" bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
printf '%s' "${expected_evidence/\"approval-challenge\":\"$CHALLENGE\"/\"approval-challenge\":\"$CHALLENGE\",\"approval-challenge\":\"$CHALLENGE\"}" >"$bad_evidence"
assert_fail "a duplicate evidence key is rejected before object construction" \
  bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
printf '%s' "${expected_evidence/\"approval-challenge\"/\"\\u0061pproval-challenge\"}" >"$bad_evidence"
assert_fail "a noncanonical escaped evidence key is rejected" \
  bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
printf 'not json' >"$bad_evidence"
assert_fail "malformed evidence JSON is rejected" \
  bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"

for login in '' '-reviewer' 'reviewer-' 'reviewer--two' 'reviewer_name' \
  'reviewer-login-that-is-more-than-thirty-nine-characters'; do
  printf '%s' "${expected_evidence/\"approver-login\":\"release-reviewer\"/\"approver-login\":\"$login\"}" >"$bad_evidence"
  assert_fail "login '$login' is rejected" bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
done
for field_and_value in \
  'run-id=0' 'run-id=01' 'run-id=-1' 'run-id=1.5' \
  'run-attempt=0' 'run-attempt=01' 'run-attempt=-1' 'run-attempt=1.5'; do
  field=${field_and_value%%=*}
  value=${field_and_value#*=}
  invalid=$(jq -cS --arg field "$field" --arg value "$value" '.[$field]=$value' "$evidence")
  printf '%s' "$invalid" >"$bad_evidence"
  assert_fail "$field value '$value' is rejected" \
    bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
done
for reviewed_at in '2026-02-30T12:00:00Z' '2026-01-01T12:00:00+00:00' \
  '2026-01-01T12:00:00.000Z' '2026-01-01T25:00:00Z'; do
  printf '%s' "${expected_evidence/\"reviewed-at\":\"$NOW\"/\"reviewed-at\":\"$reviewed_at\"}" >"$bad_evidence"
  assert_fail "review time '$reviewed_at' is rejected" bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
done
for reviewed_at in '2000-01-01T00:00:00Z' '2099-01-01T00:00:00Z'; do
  printf '%s' "${expected_evidence/\"reviewed-at\":\"$NOW\"/\"reviewed-at\":\"$reviewed_at\"}" >"$bad_evidence"
  assert_pass "archived review time '$reviewed_at' is not re-aged" \
    bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
done

for field in approval-challenge image-digest screenshot-sha256 source-revision release-tag; do
  invalid=$(jq -cS --arg field "$field" '.[$field]="invalid"' "$evidence")
  printf '%s' "$invalid" >"$bad_evidence"
  assert_fail "invalid evidence $field grammar is rejected" \
    bash "$CHECK" validate-pair "$bad_image" "$bad_evidence"
done

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
