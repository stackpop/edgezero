#!/usr/bin/env bash
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
VERIFY="$DIR/../../../docker/build-app-cli/verify-toolchain.sh"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/edgezero verify-toolchain.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

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
  local description=$1 log="$WORK/assert-pass.log"
  shift
  if "$@" >"$log" 2>&1; then
    ok "$description"
  else
    cat "$log" >&2
    no "$description"
  fi
}
assert_fail() {
  local description=$1
  shift
  if "$@" >/dev/null 2>&1; then no "$description"; else ok "$description"; fi
}

write_executable() {
  local path=$1
  shift
  mkdir -p "$(dirname -- "$path")"
  printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' "$@" >"$path"
  chmod 0755 "$path"
}

make_root() {
  local name=$1
  ROOT="$WORK/$name"
  STATE="$ROOT/state"
  mkdir -p \
    "$ROOT/usr/local/cargo/bin" \
    "$ROOT/usr/local/bin" \
    "$ROOT/usr/local/share/edgezero" \
    "$ROOT/lib64" \
    "$ROOT/opt/edgezero/runtime-lib" \
    "$ROOT/usr/bin" \
    "$STATE"

  cat >"$STATE/rustc" <<'EOF'
rustc 1.95.0 (59807616e 2026-04-14)
binary: rustc
commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860
commit-date: 2026-04-14
host: x86_64-unknown-linux-gnu
release: 1.95.0
LLVM version: 22.1.2
EOF
  cat >"$STATE/fastly" <<'EOF'
Fastly CLI version v15.1.0 (e58c0f5e)
Built with go version go1.26.3 linux/amd64 (2026-09-08)
EOF
  printf 'sccache 0.10.0\n' >"$STATE/sccache"
  printf 'wasm32-wasip1\n' >"$STATE/targets"
  printf '0\n' >"$STATE/compile-status"
  printf 'wasm\n' >"$STATE/compile-output"
  printf '0\n' >"$STATE/validator-status"
  printf '1001\n' >"$STATE/uid"
  printf '1001\n' >"$STATE/gid"
  printf '1\n' >"$STATE/touch-status"
  printf 'EDGEZERO_ENV_PROBE=space quote" slash\\ dollar$ hash# equals= unicode-\303\251\n' \
    >"$STATE/env-output"
  printf '#![no_std]\n' >"$ROOT/usr/local/share/edgezero/wasm-smoke.rs"
  printf loader >"$ROOT/lib64/ld-linux-x86-64.so.2"

  # shellcheck disable=SC2016 # These lines are literal bodies for fake executables.
  write_executable "$ROOT/usr/local/cargo/bin/rustup" \
    'if [[ "$RUSTUP_HOME" != "$FAKE_ROOT/usr/local/rustup" ]]; then exit 91; fi' \
    'if [[ "$RUSTUP_TOOLCHAIN" != "1.95.0-x86_64-unknown-linux-gnu" ]]; then exit 92; fi' \
    'if [[ "$(basename -- "$0")" == rustc ]]; then' \
    '  if [[ "$*" == "--version --verbose" ]]; then cat "$FAKE_STATE/rustc"; exit 0; fi' \
    '  [[ "$(cat "$FAKE_STATE/compile-status")" == 0 ]] || exit 1' \
    '  case " $* " in *" --crate-type cdylib "*) ;; *) exit 93 ;; esac' \
    '  output=' \
    '  while (($#)); do if [[ "$1" == -o ]]; then output=$2; shift 2; else shift; fi; done' \
    '  [[ -n "$output" ]]' \
    '  if [[ "$(cat "$FAKE_STATE/compile-output")" == wasm ]]; then printf "\\0asm" >"$output"; else printf bad >"$output"; fi' \
    'else' \
    '  [[ "$*" == "target list --installed" ]]' \
    '  cat "$FAKE_STATE/targets"' \
    'fi'
  ln -s rustup "$ROOT/usr/local/cargo/bin/rustc"
  # shellcheck disable=SC2016
  write_executable "$ROOT/usr/local/bin/fastly" \
    '[[ "$*" == "--quiet version" ]]' \
    'cat "$FAKE_STATE/fastly"'
  # shellcheck disable=SC2016
  write_executable "$ROOT/usr/local/bin/sccache" \
    '[[ "$*" == "--version" ]]' \
    'cat "$FAKE_STATE/sccache"'
  # shellcheck disable=SC2016
  write_executable "$ROOT/usr/local/bin/edgezero-provenance-validator" \
    '[[ "$*" == "self-test --fixtures /usr/local/share/edgezero/provenance-fixtures" ]]' \
    'exit "$(cat "$FAKE_STATE/validator-status")"'
  # shellcheck disable=SC2016
  write_executable "$ROOT/usr/bin/id" \
    'case "$1" in -u) cat "$FAKE_STATE/uid" ;; -g) cat "$FAKE_STATE/gid" ;; *) exit 1 ;; esac'
  # shellcheck disable=SC2016
  write_executable "$ROOT/usr/bin/touch" \
    'exit "$(cat "$FAKE_STATE/touch-status")"'
  # shellcheck disable=SC2016
  write_executable "$ROOT/usr/bin/env" \
    'cat "$FAKE_STATE/env-output"'
}

run_verify() {
  local fake_root
  fake_root=$(cd -- "$ROOT" && pwd -P)
  FAKE_ROOT="$fake_root" \
    FAKE_STATE="$STATE" \
    RUSTUP_HOME=/hostile/rustup \
    RUSTUP_TOOLCHAIN=nightly \
    TMPDIR="$WORK/tmp" \
    bash "$VERIFY" --root "$ROOT"
}

echo "== build-container toolchain verification =="
mkdir "$WORK/tmp"

make_root exact
assert_pass "exact toolchain and runtime capabilities pass" run_verify

make_root rust-prerelease
sed -i.bak 's/release: 1.95.0/release: 1.95.0-beta.1/' "$STATE/rustc"
rm "$STATE/rustc.bak"
assert_fail "a prerelease Rust version is rejected" run_verify

make_root rust-extra
sed -i.bak '1s/$/ extra/' "$STATE/rustc"
rm "$STATE/rustc.bak"
assert_fail "extra Rust version text is rejected" run_verify

make_root rust-missing
sed -i.bak '/^release:/d' "$STATE/rustc"
rm "$STATE/rustc.bak"
assert_fail "missing Rust release output is rejected" run_verify

make_root fastly-malformed
printf 'Fastly CLI version 15.1.0\n' >"$STATE/fastly"
assert_fail "malformed Fastly version output is rejected" run_verify

make_root fastly-wrong
sed -i.bak 's/v15.1.0/v15.1.1/' "$STATE/fastly"
rm "$STATE/fastly.bak"
assert_fail "a different Fastly version is rejected" run_verify

make_root sccache-extra
printf 'sccache 0.10.0 extra\n' >"$STATE/sccache"
assert_fail "extra sccache version text is rejected" run_verify

make_root target-missing
: >"$STATE/targets"
assert_fail "an absent wasm32-wasip1 target is rejected" run_verify

make_root compile-failure
printf '1\n' >"$STATE/compile-status"
assert_fail "a failed minimal wasm compile is rejected" run_verify

make_root invalid-wasm
printf 'bad\n' >"$STATE/compile-output"
assert_fail "invalid wasm magic is rejected" run_verify

make_root validator-failure
printf '1\n' >"$STATE/validator-status"
assert_fail "a validator self-test failure is rejected" run_verify

make_root wrong-uid
printf '1002\n' >"$STATE/uid"
assert_fail "a wrong uid is rejected" run_verify

make_root wrong-gid
printf '1002\n' >"$STATE/gid"
assert_fail "a wrong gid is rejected" run_verify

make_root wrong-rustc-alias
rm "$ROOT/usr/local/cargo/bin/rustc"
ln -s ../../bin/fastly "$ROOT/usr/local/cargo/bin/rustc"
assert_fail "a rustc proxy pointing anywhere except rustup is rejected" run_verify

make_root symlinked-loader-parent
mkdir -p "$ROOT/usr/lib64"
mv "$ROOT/lib64/ld-linux-x86-64.so.2" "$ROOT/usr/lib64/ld-linux-x86-64.so.2"
rmdir "$ROOT/lib64"
ln -s usr/lib64 "$ROOT/lib64"
assert_fail "a symlinked /lib64 acquisition path is rejected" run_verify

make_root writable-root
printf '0\n' >"$STATE/touch-status"
assert_fail "a writable root filesystem is rejected" run_verify

make_root missing-env
rm "$ROOT/usr/bin/env"
assert_fail "a missing GNU env is rejected" run_verify

make_root incompatible-env
printf 'EDGEZERO_ENV_PROBE=unexpanded\n' >"$STATE/env-output"
assert_fail "an env without expansion-before-clear semantics is rejected" run_verify

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
