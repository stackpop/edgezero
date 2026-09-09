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
    "$ROOT/etc" \
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
Built with go version go1.26.3 linux/amd64 (2026-09-09)
EOF
  printf 'sccache 0.10.0\n' >"$STATE/sccache"
  printf 'wasm32-wasip1\nx86_64-unknown-linux-gnu\n' >"$STATE/targets"
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
  printf libc >"$ROOT/opt/edgezero/runtime-lib/libc.so.6"
  printf libgcc >"$ROOT/opt/edgezero/runtime-lib/libgcc_s.so.1"
  printf libm >"$ROOT/opt/edgezero/runtime-lib/libm.so.6"
  cat >"$STATE/elf-header" <<'EOF'
Class: ELF64
Data: 2's complement, little endian
Version: 1 (current)
OS/ABI: UNIX - GNU
ABI Version: 0
Type: DYN (Shared object file)
Machine: Advanced Micro Devices X86-64
EOF
  : >"$STATE/program-loader"
  : >"$STATE/program-libc.so.6"
  : >"$STATE/program-libgcc_s.so.1"
  : >"$STATE/program-libm.so.6"
  printf 'SONAME=libc.so.6\nNEEDED=ld-linux-x86-64.so.2\n' >"$STATE/dynamic-libc.so.6"
  printf 'SONAME=libgcc_s.so.1\nNEEDED=libc.so.6\n' >"$STATE/dynamic-libgcc_s.so.1"
  printf 'SONAME=libm.so.6\nNEEDED=libc.so.6\nNEEDED=ld-linux-x86-64.so.2\n' >"$STATE/dynamic-libm.so.6"
  printf 'SONAME=ld-linux-x86-64.so.2\n' >"$STATE/dynamic-loader"
  printf 'expected\n' >"$STATE/closure-hash-status"

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
  # shellcheck disable=SC2016
  write_executable "$ROOT/usr/bin/x86_64-linux-gnu-readelf" \
    'base=$(basename -- "$2")' \
    '[[ "$base" != ld-linux-x86-64.so.2 ]] || base=loader' \
    'case "$1" in' \
    '  -h) cat "$FAKE_STATE/elf-header" ;;' \
    '  -l) cat "$FAKE_STATE/program-$base" ;;' \
    '  -d) cat "$FAKE_STATE/dynamic-$base" ;;' \
    '  *) exit 1 ;;' \
    'esac'
  # shellcheck disable=SC2016
  write_executable "$ROOT/usr/bin/sha256sum" \
    '[[ "$(cat "$FAKE_STATE/closure-hash-status")" == expected ]] || { printf "0%.0s" {1..64}; printf "  %s\n" "$1"; exit 0; }' \
    'case "$(basename -- "$1")" in' \
    '  ld-linux-x86-64.so.2) hash=02bcda52c1a5dfc236f94d9e5255b4a0e26347d8a372a5223b650e31f291ce3c ;;' \
    '  libc.so.6) hash=6b4a45352fd0c540a9c7c718f35ce8c8e46a4e482f9d3885a910c32d1a0e1421 ;;' \
    '  libgcc_s.so.1) hash=2bd1552c47799ef67e701e81d4383061fd76059868e446e63560f0dd0d5ec14e ;;' \
    '  libm.so.6) hash=7f2ca87f652f56b094462474b076749e90e689d0ecb9cb63c7679820b271b4e7 ;;' \
    '  *) exit 1 ;;' \
    'esac' \
    'printf "%s  %s\n" "$hash" "$1"'
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

make_root rust-malformed
sed -i.bak 's/release: 1.95.0/release: 1.95/' "$STATE/rustc"
rm "$STATE/rustc.bak"
assert_fail "malformed Rust version output is rejected" run_verify

make_root fastly-malformed
printf 'Fastly CLI version 15.1.0\n' >"$STATE/fastly"
assert_fail "malformed Fastly version output is rejected" run_verify

make_root fastly-prerelease
sed -i.bak 's/v15.1.0/v15.1.0-rc.1/' "$STATE/fastly"
rm "$STATE/fastly.bak"
assert_fail "a prerelease Fastly version is rejected" run_verify

make_root fastly-extra
printf 'unexpected\n' >>"$STATE/fastly"
assert_fail "extra Fastly version text is rejected" run_verify

make_root fastly-missing
: >"$STATE/fastly"
assert_fail "missing Fastly version output is rejected" run_verify

make_root fastly-wrong
sed -i.bak 's/v15.1.0/v15.1.1/' "$STATE/fastly"
rm "$STATE/fastly.bak"
assert_fail "a different Fastly version is rejected" run_verify

make_root sccache-prerelease
printf 'sccache 0.10.0-rc.1\n' >"$STATE/sccache"
assert_fail "a prerelease sccache version is rejected" run_verify

make_root sccache-extra
printf 'sccache 0.10.0 extra\n' >"$STATE/sccache"
assert_fail "extra sccache version text is rejected" run_verify

make_root sccache-missing
: >"$STATE/sccache"
assert_fail "missing sccache version output is rejected" run_verify

make_root sccache-malformed
printf 'sccache version 0.10.0\n' >"$STATE/sccache"
assert_fail "malformed sccache version output is rejected" run_verify

make_root sccache-wrong
printf 'sccache 0.10.1\n' >"$STATE/sccache"
assert_fail "a different sccache version is rejected" run_verify

make_root target-missing
: >"$STATE/targets"
assert_fail "an absent wasm32-wasip1 target is rejected" run_verify

make_root target-extra
printf 'aarch64-unknown-linux-gnu\nwasm32-wasip1\nx86_64-unknown-linux-gnu\n' >"$STATE/targets"
assert_fail "an extra installed target is rejected" run_verify

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

make_root bookworm-libc-interpreter
printf '[Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]\n' \
  >"$STATE/program-libc.so.6"
assert_pass "Bookworm libc may carry the exact supported PT_INTERP" run_verify

make_root missing-runtime-member
rm "$ROOT/opt/edgezero/runtime-lib/libc.so.6"
assert_fail "a missing flat runtime-library member is rejected" run_verify

make_root extra-runtime-member
printf rogue >"$ROOT/opt/edgezero/runtime-lib/librogue.so.1"
assert_fail "an extra flat runtime-library member is rejected" run_verify

make_root hidden-runtime-member
printf rogue >"$ROOT/opt/edgezero/runtime-lib/.hidden.so"
assert_fail "a hidden flat runtime-library member is rejected" run_verify

make_root linked-runtime-member
rm "$ROOT/opt/edgezero/runtime-lib/libm.so.6"
ln -s libc.so.6 "$ROOT/opt/edgezero/runtime-lib/libm.so.6"
assert_fail "a symlinked flat runtime-library member is rejected" run_verify

make_root replaced-runtime-member
printf 'wrong\n' >"$STATE/closure-hash-status"
assert_fail "a byte-replaced runtime closure member is rejected" run_verify

make_root wrong-runtime-soname
printf 'SONAME=libwrong.so.6\nNEEDED=libc.so.6\n' >"$STATE/dynamic-libm.so.6"
assert_fail "a filename and SONAME disagreement is rejected" run_verify

make_root runtime-interpreter
printf '[Requesting program interpreter: /attacker/ld.so]\n' \
  >"$STATE/program-libm.so.6"
assert_fail "a runtime library carrying a foreign PT_INTERP is rejected" run_verify

make_root hardlinked-loader
ln "$ROOT/lib64/ld-linux-x86-64.so.2" "$ROOT/lib64/loader-alias"
assert_fail "a multiply linked dynamic interpreter is rejected" run_verify

make_root wrong-loader-soname
printf 'SONAME=attacker-loader.so\n' >"$STATE/dynamic-loader"
assert_fail "a dynamic interpreter carrying the wrong SONAME is rejected" run_verify

make_root preload-present
printf '/attacker/lib.so\n' >"$ROOT/etc/ld.so.preload"
assert_fail "a system preload file is rejected" run_verify

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
