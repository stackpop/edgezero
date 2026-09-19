#!/usr/bin/env bash
set -euo pipefail

# Contract tests for the provider-neutral Rust build-cache setup action.
#
# These tests exercise only local files and action metadata. They require no
# network, credentials, or provider CLI.

ACTION_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/edgezero-rust-cache-test.XXXXXX")
trap 'rm -rf -- "$WORK_DIR"' EXIT

fail() {
  printf 'setup-rust-build-cache test failed: %s\n' "$*" >&2
  exit 1
}

assert_line() {
  local file="$1" expected="$2"
  grep -Fqx -- "$expected" "$file" ||
    fail "expected '$expected' in $file"
}

workspace="$WORK_DIR/workspace"
runner_temp="$WORK_DIR/runner"
mkdir -p "$workspace/project" "$runner_temp"
printf '%s\n' '# generic lockfile fixture' >"$workspace/project/Cargo.lock"
: >"$WORK_DIR/output"
: >"$WORK_DIR/env"

GITHUB_WORKSPACE="$workspace" \
  RUNNER_TEMP="$runner_temp" \
  GITHUB_OUTPUT="$WORK_DIR/output" \
  GITHUB_ENV="$WORK_DIR/env" \
  EDGEZERO__RUST_CACHE__APP_NAME=example-app \
  EDGEZERO__RUST_CACHE__CACHE_TARGET=true \
  EDGEZERO__RUST_CACHE__WORKING_DIRECTORY=project \
  "$ACTION_DIR/scripts/prepare.sh"

if command -v sha256sum >/dev/null 2>&1; then
  expected_sha=$(sha256sum "$workspace/project/Cargo.lock" | awk '{ print $1 }')
else
  expected_sha=$(shasum -a 256 "$workspace/project/Cargo.lock" | awk '{ print $1 }')
fi
assert_line "$WORK_DIR/output" "cargo-lock-sha256=$expected_sha"
assert_line "$WORK_DIR/output" 'cache-namespace=example-app'
expected_target=$(realpath "$runner_temp/edgezero-rust-cache/example-app/target")
assert_line "$WORK_DIR/output" "cargo-target-dir=$expected_target"
assert_line "$WORK_DIR/env" 'RUSTC_WRAPPER=sccache'
assert_line "$WORK_DIR/env" 'SCCACHE_GHA_ENABLED=true'
assert_line "$WORK_DIR/env" 'CARGO_INCREMENTAL=0'
assert_line "$WORK_DIR/env" "CARGO_TARGET_DIR=$expected_target"
assert_line "$WORK_DIR/env" "EDGEZERO__RUST_CACHE__TARGET_DIR=$expected_target"

: >"$WORK_DIR/output"
: >"$WORK_DIR/env"
GITHUB_WORKSPACE="$workspace" \
  RUNNER_TEMP="$runner_temp" \
  GITHUB_OUTPUT="$WORK_DIR/output" \
  GITHUB_ENV="$WORK_DIR/env" \
  EDGEZERO__RUST_CACHE__APP_NAME=example-app \
  EDGEZERO__RUST_CACHE__CACHE_TARGET=false \
  EDGEZERO__RUST_CACHE__WORKING_DIRECTORY=project \
  "$ACTION_DIR/scripts/prepare.sh"
assert_line "$WORK_DIR/output" 'cache-namespace=example-app'
if grep -Fq 'cargo-target-dir=' "$WORK_DIR/output"; then
  fail "published a Cargo target directory while target caching was disabled"
fi
if grep -Eq '^(CARGO_TARGET_DIR|EDGEZERO__RUST_CACHE__TARGET_DIR)=' "$WORK_DIR/env"; then
  fail "enabled Cargo target caching while target caching was disabled"
fi

for invalid in missing ../outside /; do
  : >"$WORK_DIR/output"
  : >"$WORK_DIR/env"
  if GITHUB_WORKSPACE="$workspace" \
    GITHUB_OUTPUT="$WORK_DIR/output" \
    GITHUB_ENV="$WORK_DIR/env" \
    EDGEZERO__RUST_CACHE__APP_NAME=example-app \
    EDGEZERO__RUST_CACHE__CACHE_TARGET=false \
    EDGEZERO__RUST_CACHE__WORKING_DIRECTORY="$invalid" \
    "$ACTION_DIR/scripts/prepare.sh" >/dev/null 2>&1; then
    fail "accepted invalid working-directory '$invalid'"
  fi
  [[ ! -s "$WORK_DIR/output" ]] ||
    fail "published a cache key for invalid working-directory '$invalid'"
  [[ ! -s "$WORK_DIR/env" ]] ||
    fail "enabled compiler caching for invalid working-directory '$invalid'"
done

for invalid_name in '' 'Example App' '../example' 'example/app'; do
  : >"$WORK_DIR/output"
  : >"$WORK_DIR/env"
  if GITHUB_WORKSPACE="$workspace" \
    RUNNER_TEMP="$runner_temp" \
    GITHUB_OUTPUT="$WORK_DIR/output" \
    GITHUB_ENV="$WORK_DIR/env" \
    EDGEZERO__RUST_CACHE__APP_NAME="$invalid_name" \
    EDGEZERO__RUST_CACHE__CACHE_TARGET=false \
    EDGEZERO__RUST_CACHE__WORKING_DIRECTORY=project \
    "$ACTION_DIR/scripts/prepare.sh" >/dev/null 2>&1; then
    fail "accepted invalid app-name '$invalid_name'"
  fi
  [[ ! -s "$WORK_DIR/output" ]] || fail "published outputs for invalid app-name '$invalid_name'"
  [[ ! -s "$WORK_DIR/env" ]] || fail "enabled caching for invalid app-name '$invalid_name'"
done

: >"$WORK_DIR/output"
: >"$WORK_DIR/env"
if GITHUB_WORKSPACE="$workspace" \
  RUNNER_TEMP="$runner_temp" \
  GITHUB_OUTPUT="$WORK_DIR/output" \
  GITHUB_ENV="$WORK_DIR/env" \
  EDGEZERO__RUST_CACHE__APP_NAME=example-app \
  EDGEZERO__RUST_CACHE__CACHE_TARGET=yes \
  EDGEZERO__RUST_CACHE__WORKING_DIRECTORY=project \
  "$ACTION_DIR/scripts/prepare.sh" >/dev/null 2>&1; then
  fail "accepted invalid cache-target value 'yes'"
fi
[[ ! -s "$WORK_DIR/output" ]] || fail "published outputs for invalid cache-target"
[[ ! -s "$WORK_DIR/env" ]] || fail "enabled caching for invalid cache-target"

action="$ACTION_DIR/action.yml"
[[ -f "$action" ]] || fail "missing action.yml"
grep -Eq '^  app-name:$' "$action" || fail "action does not expose the required app-name input"
if grep -Eq '^  application-name:$' "$action"; then
  fail "action still exposes the inconsistent application-name input"
fi
grep -Fq 'actions/cache@v5' "$action" || fail "Cargo source cache is not pinned to actions/cache v5"
grep -Fq 'mozilla-actions/sccache-action@fc920bf0ec8de6ee65d409111f7ec508035751ba' "$action" ||
  fail "sccache setup action is not pinned to the reviewed v0.0.11 commit"
grep -Eq '^[[:space:]]+version:[[:space:]]+v0\.16\.0[[:space:]]*$' "$action" ||
  fail "sccache binary version is not pinned to v0.16.0"
tilde='~'
for path in "$tilde/.cargo/registry/index/" "$tilde/.cargo/registry/cache/" "$tilde/.cargo/git/db/"; do
  grep -Fq "$path" "$action" || fail "Cargo source cache omits $path"
done
grep -Fq "steps.prepare.outputs['cargo-lock-sha256']" "$action" ||
  fail "Cargo source cache key is not bound to the validated Cargo.lock"
grep -Fq "steps.prepare.outputs['cache-namespace']" "$action" ||
  fail "cache keys are not scoped to the validated application name"
grep -Fq "steps.prepare.outputs['cargo-target-dir']" "$action" ||
  fail "Cargo target cache does not use the validated target directory"
grep -Fq "inputs['cache-target'] == 'true'" "$action" ||
  fail "Cargo target cache is not gated by the cache-target input"
grep -Fq '${{ github.sha }}' "$action" ||
  fail "Cargo target cache primary key is not bound to the source revision"

printf 'setup-rust-build-cache action tests passed\n'
