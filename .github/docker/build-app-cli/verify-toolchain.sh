#!/usr/bin/env bash
set -euo pipefail

die() {
  printf 'verify-toolchain: %s\n' "$*" >&2
  exit 1
}

root=
while (($#)); do
  (($# >= 2)) || die "missing value for $1"
  case "$1" in
    --root)
      [[ -z "$root" ]] || die "duplicate --root"
      root=$2
      ;;
    *) die "unknown argument: $1" ;;
  esac
  shift 2
done

[[ -n "$root" ]] || die "--root is required"
root=$(cd -- "$root" 2>/dev/null && pwd -P) || die "root is not a directory"
if [[ "$root" == "/" ]]; then
  prefix=''
else
  prefix=${root%/}
fi

rustc="$prefix/usr/local/cargo/bin/rustc"
rustup="$prefix/usr/local/cargo/bin/rustup"
fastly="$prefix/usr/local/bin/fastly"
sccache="$prefix/usr/local/bin/sccache"
validator="$prefix/usr/local/bin/edgezero-provenance-validator"
env_bin="$prefix/usr/bin/env"
id_bin="$prefix/usr/bin/id"
touch_bin="$prefix/usr/bin/touch"
fixture="$prefix/usr/local/share/edgezero/wasm-smoke.rs"
loader_dir="$prefix/lib64"
loader="$loader_dir/ld-linux-x86-64.so.2"
runtime_lib="$prefix/opt/edgezero/runtime-lib"
rustup_home="$prefix/usr/local/rustup"
rustup_toolchain=1.95.0-x86_64-unknown-linux-gnu

for executable in "$rustup" "$fastly" "$sccache" "$validator" "$env_bin" "$id_bin" "$touch_bin"; do
  [[ -f "$executable" && ! -L "$executable" && -x "$executable" ]] ||
    die "required executable is missing, linked, or not executable: $executable"
done
[[ -L "$rustc" && "$(readlink "$rustc")" == "rustup" && "$rustc" -ef "$rustup" && -x "$rustc" ]] ||
  die "rustc is not the exact rustup proxy alias"
[[ -f "$fixture" && ! -L "$fixture" ]] || die "wasm smoke fixture is missing or linked"
[[ -d "$loader_dir" && ! -L "$loader_dir" ]] || die "/lib64 is not a real directory"
[[ -f "$loader" && ! -L "$loader" ]] || die "dynamic interpreter is missing or linked"
[[ -d "$runtime_lib" && ! -L "$runtime_lib" ]] || die "runtime library root is not a real directory"

expected_rustc='rustc 1.95.0 (59807616e 2026-04-14)
binary: rustc
commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860
commit-date: 2026-04-14
host: x86_64-unknown-linux-gnu
release: 1.95.0
LLVM version: 22.1.2'
actual_rustc=$(RUSTUP_HOME="$rustup_home" RUSTUP_TOOLCHAIN="$rustup_toolchain" \
  "$rustc" --version --verbose) || die "rustc version command failed"
[[ "$actual_rustc" == "$expected_rustc" ]] || die "rustc version output differs"

expected_fastly='Fastly CLI version v15.1.0 (e58c0f5e)
Built with go version go1.26.3 linux/amd64 (2026-09-08)'
actual_fastly=$("$fastly" --quiet version) || die "Fastly version command failed"
[[ "$actual_fastly" == "$expected_fastly" ]] || die "Fastly version output differs"

actual_sccache=$("$sccache" --version) || die "sccache version command failed"
[[ "$actual_sccache" == "sccache 0.10.0" ]] || die "sccache version output differs"

installed_targets=$(RUSTUP_HOME="$rustup_home" RUSTUP_TOOLCHAIN="$rustup_toolchain" \
  "$rustup" target list --installed) || die "rustup target inspection failed"
grep -Fxq 'wasm32-wasip1' <<<"$installed_targets" || die "wasm32-wasip1 is not installed"

work=$(mktemp -d "${TMPDIR:-/tmp}/edgezero-toolchain.XXXXXX")
trap 'rm -rf "$work"' EXIT
RUSTUP_HOME="$rustup_home" RUSTUP_TOOLCHAIN="$rustup_toolchain" "$rustc" \
  --crate-name edgezero_wasm_smoke \
  --crate-type cdylib \
  --edition 2024 \
  --target wasm32-wasip1 \
  -C opt-level=0 \
  "$fixture" \
  -o "$work/wasm-smoke.wasm" || die "minimal wasm compile failed"
[[ -f "$work/wasm-smoke.wasm" && ! -L "$work/wasm-smoke.wasm" ]] ||
  die "minimal wasm compile did not create a regular output"
magic=$(od -An -tx1 -N4 "$work/wasm-smoke.wasm" | tr -d ' \n')
[[ "$magic" == "0061736d" ]] || die "minimal compile output lacks wasm magic"

"$validator" self-test --fixtures /usr/local/share/edgezero/provenance-fixtures ||
  die "validator self-test failed"

[[ "$("$id_bin" -u)" == "1001" ]] || die "runtime uid is not 1001"
[[ "$("$id_bin" -g)" == "1001" ]] || die "runtime gid is not 1001"

root_probe="$prefix/.edgezero-read-only-probe.$$"
if "$touch_bin" "$root_probe" >/dev/null 2>&1; then
  rm -f -- "$root_probe"
  die "root filesystem is writable"
fi

env_probe=$(printf 'space quote" slash\\ dollar$ hash# equals= unicode-\303\251')
# shellcheck disable=SC2016 # GNU env must expand this literal placeholder itself.
env_output=$(EDGEZERO_ENV_PROBE="$env_probe" \
  "$env_bin" -S '-i EDGEZERO_ENV_PROBE=${EDGEZERO_ENV_PROBE}') ||
  die "GNU env -S probe failed"
[[ "$env_output" == "EDGEZERO_ENV_PROBE=$env_probe" ]] ||
  die "GNU env lacks required expansion-before-clear semantics"
