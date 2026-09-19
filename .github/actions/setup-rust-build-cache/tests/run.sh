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
mkdir -p "$workspace/project"
printf '%s\n' '# generic lockfile fixture' >"$workspace/project/Cargo.lock"
: >"$WORK_DIR/output"
: >"$WORK_DIR/env"

GITHUB_WORKSPACE="$workspace" \
  GITHUB_OUTPUT="$WORK_DIR/output" \
  GITHUB_ENV="$WORK_DIR/env" \
  EDGEZERO__RUST_CACHE__WORKING_DIRECTORY=project \
  "$ACTION_DIR/scripts/prepare.sh"

if command -v sha256sum >/dev/null 2>&1; then
  expected_sha=$(sha256sum "$workspace/project/Cargo.lock" | awk '{ print $1 }')
else
  expected_sha=$(shasum -a 256 "$workspace/project/Cargo.lock" | awk '{ print $1 }')
fi
assert_line "$WORK_DIR/output" "cargo-lock-sha256=$expected_sha"
assert_line "$WORK_DIR/env" 'RUSTC_WRAPPER=sccache'
assert_line "$WORK_DIR/env" 'SCCACHE_GHA_ENABLED=true'
assert_line "$WORK_DIR/env" 'CARGO_INCREMENTAL=0'

for invalid in missing ../outside /; do
  : >"$WORK_DIR/output"
  : >"$WORK_DIR/env"
  if GITHUB_WORKSPACE="$workspace" \
    GITHUB_OUTPUT="$WORK_DIR/output" \
    GITHUB_ENV="$WORK_DIR/env" \
    EDGEZERO__RUST_CACHE__WORKING_DIRECTORY="$invalid" \
    "$ACTION_DIR/scripts/prepare.sh" >/dev/null 2>&1; then
    fail "accepted invalid working-directory '$invalid'"
  fi
  [[ ! -s "$WORK_DIR/output" ]] ||
    fail "published a cache key for invalid working-directory '$invalid'"
  [[ ! -s "$WORK_DIR/env" ]] ||
    fail "enabled compiler caching for invalid working-directory '$invalid'"
done

action="$ACTION_DIR/action.yml"
[[ -f "$action" ]] || fail "missing action.yml"
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

printf 'setup-rust-build-cache action tests passed\n'
