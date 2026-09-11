#!/usr/bin/env bash
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CHECK="$DIR/../../../docker/build-app-cli/check-image-pin.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

REPO="ghcr.io/stackpop/edgezero-build-app-cli"
DIGEST="sha256:$(printf '1%.0s' {1..64})"
SOURCE=$(printf '2%.0s' {1..40})
CHALLENGE=$(printf '3%.0s' {1..64})
SCREENSHOT="sha256:$(printf '4%.0s' {1..64})"
TAG="build-container-v7"
NOW=$(date -u '+%Y-%m-%dT%H:%M:%SZ')

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
assert_eq() {
  local description=$1 expected=$2
  shift 2
  local actual
  if ! actual=$("$@" 2>/dev/null); then
    no "$description"
  elif [[ "$actual" == "$expected" ]]; then
    ok "$description"
  else
    no "$description"
    printf '    expected: %s\n    actual:   %s\n' "$expected" "$actual" >&2
  fi
}

write_image() {
  local path=$1 content
  if [[ $# -eq 2 ]]; then
    content=$2
  else
    content="{\"repository\":\"$REPO\",\"tag\":\"$TAG\",\"digest\":\"$DIGEST\",\"image-source-revision\":\"$SOURCE\",\"provenance-protocol\":1}"
  fi
  printf '%s' "$content" >"$path"
}

write_evidence() {
  local path=$1 reviewed_at=${2:-$NOW} content
  if [[ $# -eq 3 ]]; then
    content=$3
  else
    content="{\"approval-challenge\":\"$CHALLENGE\",\"approver-login\":\"release-reviewer\",\"image-digest\":\"$DIGEST\",\"release-tag\":\"$TAG\",\"reviewed-at\":\"$reviewed_at\",\"run-attempt\":\"2\",\"run-id\":\"9007199254740993\",\"schema-version\":1,\"screenshot-sha256\":\"$SCREENSHOT\",\"source-revision\":\"$SOURCE\"}"
  fi
  printf '%s' "$content" >"$path"
}

run_check() { bash "$CHECK" "$@"; }

echo "== build container image pin validator =="

image="$WORK/image.json"
evidence="$WORK/image-release-evidence.json"
write_image "$image"
write_evidence "$evidence"

assert_pass "the exact five-field image record passes" run_check "$image"
assert_pass "the explicit validate mode passes" run_check validate "$image"
assert_eq "runtime-ref exposes only repository@digest" "$REPO@$DIGEST" \
  run_check runtime-ref "$image"
assert_eq "source-revision exposes the validated source SHA" "$SOURCE" \
  run_check source-revision "$image"
assert_eq "provenance-protocol exposes the validated integer" "1" \
  run_check provenance-protocol "$image"
assert_fail "there is no runtime tag accessor" run_check tag "$image"
assert_pass "the coherent image/evidence pair passes" run_check validate-pair "$image" "$evidence"

write_evidence "$WORK/stale-evidence.json" '2020-01-01T00:00:00Z'
assert_pass "archived evidence does not expire against a later wall clock" \
  run_check validate-pair "$image" "$WORK/stale-evidence.json"
write_evidence "$WORK/future-evidence.json" '2099-01-01T00:00:00Z'
assert_pass "archival validation leaves freshness to the successful publisher gate" \
  run_check validate-pair "$image" "$WORK/future-evidence.json"
write_evidence "$WORK/invalid-calendar-evidence.json" '2026-02-30T00:00:00Z'
assert_fail "an invalid evidence calendar instant is rejected" \
  run_check validate-pair "$image" "$WORK/invalid-calendar-evidence.json"

printf 'not json' >"$WORK/bad.json"
assert_fail "malformed JSON fails closed" run_check "$WORK/bad.json"
write_image "$WORK/duplicate.json" \
  "{\"repository\":\"$REPO\",\"repository\":\"$REPO\",\"tag\":\"$TAG\",\"digest\":\"$DIGEST\",\"image-source-revision\":\"$SOURCE\",\"provenance-protocol\":1}"
assert_fail "a duplicate top-level key is rejected" run_check "$WORK/duplicate.json"
write_image "$WORK/escaped-duplicate.json" \
  "{\"\\u0072epository\":\"$REPO\",\"repository\":\"$REPO\",\"tag\":\"$TAG\",\"digest\":\"$DIGEST\",\"image-source-revision\":\"$SOURCE\",\"provenance-protocol\":1}"
assert_fail "an escaped duplicate top-level key is rejected" run_check "$WORK/escaped-duplicate.json"
write_image "$WORK/extra.json" \
  "{\"repository\":\"$REPO\",\"tag\":\"$TAG\",\"digest\":\"$DIGEST\",\"image-source-revision\":\"$SOURCE\",\"provenance-protocol\":1,\"extra\":true}"
assert_fail "an extra field is rejected" run_check "$WORK/extra.json"
printf '%s%s' "$(<"$image")" "$(<"$image")" >"$WORK/multiple.json"
assert_fail "multiple JSON documents are rejected" run_check "$WORK/multiple.json"
printf '[]' >"$WORK/array.json"
assert_fail "a non-object document is rejected" run_check "$WORK/array.json"
printf '%*s' 4097 '' >"$WORK/oversized.json"
assert_fail "an oversized record is rejected before parsing" run_check "$WORK/oversized.json"
ln -s "$image" "$WORK/symlink.json"
assert_fail "a symlink record is rejected" run_check "$WORK/symlink.json"

for key in repository tag digest image-source-revision provenance-protocol; do
  jq "del(.\"$key\")" "$image" >"$WORK/missing-$key.json"
  assert_fail "missing $key is rejected" run_check "$WORK/missing-$key.json"
done

for mutation in \
  '.repository=1' \
  '.tag=false' \
  '.digest=null' \
  '."image-source-revision"=[]' \
  '."provenance-protocol"="1"'; do
  jq "$mutation" "$image" >"$WORK/wrong-type.json"
  assert_fail "wrong type $mutation is rejected" run_check "$WORK/wrong-type.json"
done

for value in '' 'ghcr.io/attacker/edgezero-build-app-cli'; do
  jq --arg value "$value" '.repository=$value' "$image" >"$WORK/bad-repo.json"
  assert_fail "repository '$value' is rejected" run_check "$WORK/bad-repo.json"
done

for value in latest "sha256:$(printf '0%.0s' {1..64})" \
  "sha256:$(printf 'A%.0s' {1..64})" sha256:deadbeef; do
  jq --arg value "$value" '.digest=$value' "$image" >"$WORK/bad-digest.json"
  assert_fail "digest '$value' is rejected" run_check "$WORK/bad-digest.json"
done

for value in "$(printf '0%.0s' {1..40})" "$(printf 'A%.0s' {1..40})" deadbeef; do
  jq --arg value "$value" '."image-source-revision"=$value' "$image" >"$WORK/bad-source.json"
  assert_fail "source '$value' is rejected" run_check "$WORK/bad-source.json"
done

for value in 0 2 1.5; do
  jq ".\"provenance-protocol\"=$value" "$image" >"$WORK/bad-protocol.json"
  assert_fail "protocol $value is rejected" run_check "$WORK/bad-protocol.json"
done

for value in v1 build-container-v0 build-container-v01 build-container-v1.2 latest; do
  jq --arg value "$value" '.tag=$value' "$image" >"$WORK/bad-tag.json"
  assert_fail "tag '$value' is rejected" run_check "$WORK/bad-tag.json"
done

assert_fail "a missing image cannot form a pair" \
  run_check validate-pair "$WORK/missing-image.json" "$evidence"
assert_fail "a missing evidence record cannot form a pair" \
  run_check validate-pair "$image" "$WORK/missing-evidence.json"

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
