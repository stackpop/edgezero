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
readelf_bin="$prefix/usr/bin/x86_64-linux-gnu-readelf"
sha256sum_bin="$prefix/usr/bin/sha256sum"
fixture="$prefix/usr/local/share/edgezero/wasm-smoke.rs"
loader_dir="$prefix/lib64"
loader="$loader_dir/ld-linux-x86-64.so.2"
runtime_lib="$prefix/opt/edgezero/runtime-lib"
rustup_home="$prefix/usr/local/rustup"
rustup_toolchain=1.95.0-x86_64-unknown-linux-gnu

for executable in \
  "$rustup" "$fastly" "$sccache" "$validator" "$env_bin" "$id_bin" "$touch_bin" \
  "$readelf_bin" "$sha256sum_bin"; do
  [[ -f "$executable" && ! -L "$executable" && -x "$executable" ]] ||
    die "required executable is missing, linked, or not executable: $executable"
done
[[ -L "$rustc" && "$(readlink "$rustc")" == "rustup" && "$rustc" -ef "$rustup" && -x "$rustc" ]] ||
  die "rustc is not the exact rustup proxy alias"
[[ -f "$fixture" && ! -L "$fixture" ]] || die "wasm smoke fixture is missing or linked"
[[ -d "$loader_dir" && ! -L "$loader_dir" ]] || die "/lib64 is not a real directory"
[[ -f "$loader" && ! -L "$loader" ]] || die "dynamic interpreter is missing or linked"
[[ -d "$runtime_lib" && ! -L "$runtime_lib" ]] || die "runtime library root is not a real directory"
[[ ! -e "$prefix/etc/ld.so.preload" && ! -L "$prefix/etc/ld.so.preload" ]] ||
  die "/etc/ld.so.preload must be absent"

shopt -s dotglob nullglob
loader_members=("$loader_dir"/*)
[[ "${#loader_members[@]}" == 1 && "${loader_members[0]}" == "$loader" ]] ||
  die "dynamic interpreter directory member set differs"
[[ "$(find "$loader" -links 1 -print)" == "$loader" ]] || die "dynamic interpreter is multiply linked"
runtime_members=("$runtime_lib"/*)
[[ "${#runtime_members[@]}" == 3 ]] || die "flat runtime-library member set differs"

elf_field() {
  local header=$1 field=$2
  sed -n "s|^[[:space:]]*$field:[[:space:]]*||p" <<<"$header"
}

assert_shared_object() {
  local path=$1 basename=$2 expected_hash=$3 expected_needed=$4
  local header program dynamic soname needed actual_hash interpreter
  [[ -f "$path" && ! -L "$path" ]] || die "runtime object is missing, linked, or non-regular: $basename"
  [[ "$(find "$path" -links 1 -print)" == "$path" ]] || die "runtime object is multiply linked: $basename"
  actual_hash=$("$sha256sum_bin" "$path") || die "cannot hash runtime object: $basename"
  [[ "$actual_hash" == "$expected_hash  $path" ]] || die "runtime object digest differs: $basename"
  header=$(LC_ALL=C "$readelf_bin" -h "$path") || die "cannot read ELF header: $basename"
  [[ "$(elf_field "$header" Class)" == ELF64 ]] || die "runtime object class differs: $basename"
  [[ "$(elf_field "$header" Data)" == "2's complement, little endian" ]] ||
    die "runtime object byte order differs: $basename"
  case "$(elf_field "$header" 'OS/ABI')" in
    'UNIX - GNU' | 'UNIX - System V') ;;
    *) die "runtime object OSABI differs: $basename" ;;
  esac
  [[ "$(elf_field "$header" 'ABI Version')" == 0 ]] || die "runtime object ABI version differs: $basename"
  [[ "$(elf_field "$header" Type)" == 'DYN (Shared object file)' ]] ||
    die "runtime object type differs: $basename"
  [[ "$(elf_field "$header" Machine)" == 'Advanced Micro Devices X86-64' ]] ||
    die "runtime object machine differs: $basename"
  program=$(LC_ALL=C "$readelf_bin" -l "$path") || die "cannot read program headers: $basename"
  interpreter=$(sed -n 's/.*Requesting program interpreter: \([^]]*\)].*/\1/p' <<<"$program")
  [[ -z "$interpreter" || "$interpreter" == /lib64/ld-linux-x86-64.so.2 ]] ||
    die "runtime object carries unsupported PT_INTERP: $basename"
  dynamic=$(LC_ALL=C "$readelf_bin" -d "$path") || die "cannot read dynamic tags: $basename"
  soname=$(sed -n -e 's/^SONAME=//p' -e 's/.*(SONAME).*[[]\([^]]*\)[]].*/\1/p' <<<"$dynamic")
  needed=$(sed -n -e 's/^NEEDED=//p' -e 's/.*(NEEDED).*[[]\([^]]*\)[]].*/\1/p' <<<"$dynamic")
  [[ "$soname" == "$basename" ]] || die "runtime filename and SONAME differ: $basename"
  [[ "$needed" == "$expected_needed" ]] || die "runtime dependency set differs: $basename"
}

assert_shared_object \
  "$runtime_lib/libc.so.6" libc.so.6 \
  6b4a45352fd0c540a9c7c718f35ce8c8e46a4e482f9d3885a910c32d1a0e1421 \
  ld-linux-x86-64.so.2
assert_shared_object \
  "$runtime_lib/libgcc_s.so.1" libgcc_s.so.1 \
  2bd1552c47799ef67e701e81d4383061fd76059868e446e63560f0dd0d5ec14e \
  libc.so.6
assert_shared_object \
  "$runtime_lib/libm.so.6" libm.so.6 \
  7f2ca87f652f56b094462474b076749e90e689d0ecb9cb63c7679820b271b4e7 \
  $'libc.so.6\nld-linux-x86-64.so.2'

loader_header=$(LC_ALL=C "$readelf_bin" -h "$loader") || die "cannot read dynamic interpreter header"
[[ "$(elf_field "$loader_header" Class)" == ELF64 ]] || die "dynamic interpreter class differs"
[[ "$(elf_field "$loader_header" Data)" == "2's complement, little endian" ]] ||
  die "dynamic interpreter byte order differs"
[[ "$(elf_field "$loader_header" 'OS/ABI')" == 'UNIX - GNU' ]] || die "dynamic interpreter OSABI differs"
[[ "$(elf_field "$loader_header" 'ABI Version')" == 0 ]] || die "dynamic interpreter ABI version differs"
[[ "$(elf_field "$loader_header" Type)" == 'DYN (Shared object file)' ]] ||
  die "dynamic interpreter type differs"
[[ "$(elf_field "$loader_header" Machine)" == 'Advanced Micro Devices X86-64' ]] ||
  die "dynamic interpreter machine differs"
loader_hash=$("$sha256sum_bin" "$loader") || die "cannot hash dynamic interpreter"
[[ "$loader_hash" == \
  "02bcda52c1a5dfc236f94d9e5255b4a0e26347d8a372a5223b650e31f291ce3c  $loader" ]] ||
  die "dynamic interpreter digest differs"
loader_program=$(LC_ALL=C "$readelf_bin" -l "$loader") || die "cannot read dynamic interpreter program headers"
[[ "$loader_program" != *'Requesting program interpreter:'* ]] ||
  die "dynamic interpreter unexpectedly carries PT_INTERP"
loader_dynamic=$(LC_ALL=C "$readelf_bin" -d "$loader") || die "cannot read dynamic interpreter tags"
loader_soname=$(sed -n -e 's/^SONAME=//p' -e 's/.*(SONAME).*[[]\([^]]*\)[]].*/\1/p' <<<"$loader_dynamic")
loader_needed=$(sed -n -e 's/^NEEDED=//p' -e 's/.*(NEEDED).*[[]\([^]]*\)[]].*/\1/p' <<<"$loader_dynamic")
[[ "$loader_soname" == ld-linux-x86-64.so.2 && -z "$loader_needed" ]] ||
  die "dynamic interpreter tags differ"

assert_exact_semver() {
  local label=$1 actual=$2 expected=$3
  [[ "$actual" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
    die "$label version is not an exact stable semantic version"
  [[ "$actual" == "$expected" ]] || die "$label version differs"
}

expected_rustc='rustc 1.95.0 (59807616e 2026-04-14)
binary: rustc
commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860
commit-date: 2026-04-14
host: x86_64-unknown-linux-gnu
release: 1.95.0
LLVM version: 22.1.2'
actual_rustc=$(RUSTUP_HOME="$rustup_home" RUSTUP_TOOLCHAIN="$rustup_toolchain" \
  "$rustc" --version --verbose) || die "rustc version command failed"
rust_version=$(sed -n 's/^release: //p' <<<"$actual_rustc")
assert_exact_semver Rust "$rust_version" 1.95.0
[[ "$actual_rustc" == "$expected_rustc" ]] || die "rustc version output differs"

expected_fastly='Fastly CLI version v15.1.0 (e58c0f5e)
Built with go version go1.26.3 linux/amd64 (2026-09-09)'
actual_fastly=$("$fastly" --quiet version) || die "Fastly version command failed"
fastly_version=$(sed -n 's/^Fastly CLI version v\([^ ]*\) .*/\1/p' <<<"$actual_fastly")
assert_exact_semver Fastly "$fastly_version" 15.1.0
[[ "$actual_fastly" == "$expected_fastly" ]] || die "Fastly version output differs"

actual_sccache=$("$sccache" --version) || die "sccache version command failed"
sccache_version=$(sed -n 's/^sccache //p' <<<"$actual_sccache")
assert_exact_semver sccache "$sccache_version" 0.10.0
[[ "$actual_sccache" == "sccache 0.10.0" ]] || die "sccache version output differs"

installed_targets=$(RUSTUP_HOME="$rustup_home" RUSTUP_TOOLCHAIN="$rustup_toolchain" \
  "$rustup" target list --installed) || die "rustup target inspection failed"
installed_targets=$(LC_ALL=C sort <<<"$installed_targets")
[[ "$installed_targets" == $'wasm32-wasip1\nx86_64-unknown-linux-gnu' ]] ||
  die "installed Rust target set differs"

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
