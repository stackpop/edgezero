#!/usr/bin/env bash
set -euo pipefail

die() {
  printf 'stage-build-context: %s\n' "$*" >&2
  exit 1
}

gate_root=
source_root=
gate_sha=
source_sha=
output=

while (($#)); do
  (($# >= 2)) || die "missing value for $1"
  case "$1" in
    --gate-root)
      [[ -z "$gate_root" ]] || die "duplicate --gate-root"
      gate_root=$2
      ;;
    --source-root)
      [[ -z "$source_root" ]] || die "duplicate --source-root"
      source_root=$2
      ;;
    --gate-sha)
      [[ -z "$gate_sha" ]] || die "duplicate --gate-sha"
      gate_sha=$2
      ;;
    --source-sha)
      [[ -z "$source_sha" ]] || die "duplicate --source-sha"
      source_sha=$2
      ;;
    --output)
      [[ -z "$output" ]] || die "duplicate --output"
      output=$2
      ;;
    *) die "unknown argument: $1" ;;
  esac
  shift 2
done

[[ -n "$gate_root" ]] || die "--gate-root is required"
[[ -n "$source_root" ]] || die "--source-root is required"
[[ "$gate_sha" =~ ^[0-9a-f]{40}$ ]] || die "--gate-sha must be a full lowercase SHA"
[[ "$source_sha" =~ ^[0-9a-f]{40}$ ]] || die "--source-sha must be a full lowercase SHA"
[[ -n "$output" ]] || die "--output is required"

canonical_repo() {
  local root=$1
  local canonical top
  canonical=$(cd -- "$root" 2>/dev/null && pwd -P) || return 1
  top=$(git -C "$canonical" rev-parse --show-toplevel 2>/dev/null) || return 1
  top=$(cd -- "$top" 2>/dev/null && pwd -P) || return 1
  [[ "$canonical" == "$top" ]] || return 1
  printf '%s\n' "$canonical"
}

gate_root=$(canonical_repo "$gate_root") || die "gate root is not a canonical repository root"
source_root=$(canonical_repo "$source_root") || die "source root is not a canonical repository root"
[[ "$gate_root" != "$source_root" ]] || die "gate and source roots must differ"

assert_checkout() {
  local root=$1 expected=$2 role=$3 head sparse
  head=$(git --no-replace-objects -C "$root" rev-parse --verify HEAD 2>/dev/null) ||
    die "$role HEAD is unavailable"
  [[ "$head" == "$expected" ]] || die "$role HEAD does not match its supplied SHA"
  [[ -z "$(git --no-replace-objects -C "$root" status --porcelain=v1 --untracked-files=all)" ]] ||
    die "$role checkout is dirty"
  sparse=$(git --no-replace-objects -C "$root" config --bool core.sparseCheckout 2>/dev/null || true)
  [[ "$sparse" != "true" ]] || die "$role checkout is sparse"
}

assert_checkout "$gate_root" "$gate_sha" gate
assert_checkout "$source_root" "$source_sha" source
git --no-replace-objects -C "$source_root" merge-base --is-ancestor "$gate_sha" "$source_sha" ||
  die "gate SHA is not an ancestor of source SHA"

[[ ! -e "$output" && ! -L "$output" ]] || die "output already exists"
output_parent=$(cd -- "$(dirname -- "$output")" 2>/dev/null && pwd -P) ||
  die "output parent does not exist"
output_name=$(basename -- "$output")
[[ "$output_name" != "." && "$output_name" != ".." && "$output_name" != */* ]] ||
  die "invalid output basename"
output="$output_parent/$output_name"

is_beneath() {
  local child=$1 parent=$2
  [[ "$child" == "$parent" || "$child" == "$parent/"* ]]
}

! is_beneath "$output" "$gate_root" || die "output must be outside the gate root"
! is_beneath "$output" "$source_root" || die "output must be outside the source root"

manifest=.github/docker/build-app-cli/image-context-paths.txt
gate_manifest=.github/docker/build-app-cli/gate-paths.txt
[[ -f "$gate_root/$manifest" && ! -L "$gate_root/$manifest" ]] || die "context manifest is not regular"
[[ -f "$gate_root/$gate_manifest" && ! -L "$gate_root/$gate_manifest" ]] || die "gate manifest is not regular"

valid_path() {
  local path=$1 component
  [[ -n "$path" && "$path" != /* && "$path" != */ && "$path" != *//* && "$path" != *\\* ]] ||
    return 1
  while IFS= read -r component; do
    [[ -n "$component" && "$component" != "." && "$component" != ".." ]] || return 1
  done < <(printf '%s' "$path" | tr '/' '\n')
}

canonical_manifest() {
  local path=$1 previous='' line=''
  [[ -s "$path" ]] || return 1
  [[ "$(tail -c 1 "$path" | wc -l | tr -d ' ')" == "1" ]] || return 1
  while IFS= read -r line; do
    valid_path "$line" || return 1
    [[ -z "$previous" || "$previous" < "$line" ]] || return 1
    previous=$line
  done <"$path"
}

canonical_manifest "$gate_root/$manifest" || die "context manifest is not canonical"
canonical_manifest "$gate_root/$gate_manifest" || die "gate manifest is not canonical"

expected=$(mktemp "${TMPDIR:-/tmp}/edgezero-image-context.XXXXXX")
cleanup_expected() {
  rm -f -- "$expected"
}
trap cleanup_expected EXIT

{
  printf '%s\n' \
    .dockerignore \
    .github/actions/deploy-fastly/versions.json \
    .github/docker/build-app-cli/Dockerfile \
    .github/docker/build-app-cli/fixtures/gnu-smoke.rs \
    .github/docker/build-app-cli/fixtures/wasm-smoke.rs \
    .github/docker/build-app-cli/image-context-paths.txt \
    .github/docker/build-app-cli/provenance.schema.json \
    .github/docker/build-app-cli/verify-toolchain.sh \
    .tool-versions
  git --no-replace-objects -C "$gate_root" ls-files -- \
    .github/docker/build-app-cli/fixtures/provenance \
    .github/tools/edgezero-provenance-validator
} | LC_ALL=C sort >"$expected"

cmp -s "$expected" "$gate_root/$manifest" || die "context manifest does not match its closed inputs"

git_mode() {
  local root=$1 path=$2 record mode stage indexed_path
  record=$(git --no-replace-objects -C "$root" ls-files --stage -- "$path") || return 1
  [[ -n "$record" && "$record" != *$'\n'* ]] || return 1
  IFS=$' \t' read -r mode _ stage indexed_path <<<"$record"
  [[ "$stage" == "0" && "$indexed_path" == "$path" ]] || return 1
  [[ "$mode" == "100644" || "$mode" == "100755" ]] || return 1
  printf '%s\n' "$mode"
}

link_count() {
  if stat -c '%h' -- "$1" >/dev/null 2>&1; then
    stat -c '%h' -- "$1"
  else
    stat -f '%l' -- "$1"
  fi
}

assert_not_sparse() {
  local path=$1 size blocks
  if stat -c '%s %b' -- "$path" >/dev/null 2>&1; then
    read -r size blocks < <(stat -c '%s %b' -- "$path")
  else
    read -r size blocks < <(stat -f '%z %b' -- "$path")
  fi
  ((size == 0 || blocks * 512 >= size))
}

while IFS= read -r path; do
  grep -Fqx -- "$path" "$gate_root/$gate_manifest" || die "$path is absent from the gate manifest"
  gate_mode=$(git_mode "$gate_root" "$path") || die "$path has an invalid gate index mode"
  source_mode=$(git_mode "$source_root" "$path") || die "$path has an invalid source index mode"
  [[ "$gate_mode" == "$source_mode" ]] || die "$path mode differs between gate and source"
  [[ -f "$gate_root/$path" && ! -L "$gate_root/$path" ]] || die "$path is not a regular gate file"
  [[ -f "$source_root/$path" && ! -L "$source_root/$path" ]] || die "$path is not a regular source file"
  [[ "$(link_count "$gate_root/$path")" == "1" ]] || die "$path gate file has multiple links"
  [[ "$(link_count "$source_root/$path")" == "1" ]] || die "$path source file has multiple links"
  assert_not_sparse "$gate_root/$path" || die "$path gate file is sparse"
  assert_not_sparse "$source_root/$path" || die "$path source file is sparse"
  cmp -s "$gate_root/$path" "$source_root/$path" || die "$path differs between gate and source"
done <"$gate_root/$manifest"

mkdir -- "$output"
complete=false
cleanup_output() {
  if [[ "$complete" != true ]]; then
    rm -rf -- "$output"
  fi
}
trap 'cleanup_output; cleanup_expected' EXIT

while IFS= read -r path; do
  mkdir -p -- "$output/$(dirname -- "$path")"
  cp -- "$gate_root/$path" "$output/$path"
  mode=$(git_mode "$gate_root" "$path") || die "$path changed while staging"
  if [[ "$mode" == "100755" ]]; then
    chmod 0755 "$output/$path"
  else
    chmod 0644 "$output/$path"
  fi
done <"$gate_root/$manifest"

actual=$(mktemp "${TMPDIR:-/tmp}/edgezero-image-context-actual.XXXXXX")
find "$output" -type f -print | sed "s#^$output/##" | LC_ALL=C sort >"$actual"
cmp -s "$actual" "$gate_root/$manifest" || {
  rm -f -- "$actual"
  die "staged context inventory differs from manifest"
}
rm -f -- "$actual"

assert_checkout "$gate_root" "$gate_sha" gate
assert_checkout "$source_root" "$source_sha" source
complete=true
