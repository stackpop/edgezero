#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

validate_member_path() {
  local path="$1" label="$2" part
  [[ "$path" =~ ^[A-Za-z0-9._/-]+$ ]] || fail "$label has an invalid path"
  case "$path" in
    /* | *\\* | */ | ./* | *//* ) fail "$label has a non-normalized path" ;;
  esac
  IFS=/ read -r -a parts <<<"$path"
  for part in "${parts[@]}"; do
    [[ -n "$part" && "$part" != "." && "$part" != ".." ]] ||
      fail "$label has a traversing or non-normalized path"
  done
}

assert_exact_keys() {
  local file="$1" filter="$2" label="$3"
  jq -e "$filter" "$file" >/dev/null 2>&1 || fail "release.json has an invalid $label schema"
}

validate_release_json_syntax() {
  local file="$1" status=0
  python3 - "$file" 2>/dev/null <<'PY' || status=$?
import json
import sys


class DuplicateField(ValueError):
    pass


class InvalidConstant(ValueError):
    pass


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise DuplicateField
        result[key] = value
    return result


def reject_constant(_value):
    raise InvalidConstant


try:
    with open(sys.argv[1], encoding="utf-8") as source:
        document = json.load(
            source,
            object_pairs_hook=unique_object,
            parse_constant=reject_constant,
        )
except DuplicateField:
    sys.exit(20)
except (InvalidConstant, json.JSONDecodeError, OSError, UnicodeError):
    sys.exit(21)

if isinstance(document, dict) and "format" in document:
    if type(document["format"]) is not int or document["format"] != 1:
        sys.exit(22)
if isinstance(document, dict):
    if "lifecycle_protocol" not in document or type(document["lifecycle_protocol"]) is not int:
        sys.exit(23)
    if document["lifecycle_protocol"] != 1:
        sys.exit(24)
PY

  case "$status" in
    0) ;;
    20) fail "release.json contains a duplicate field" ;;
    21) fail "release.json is not valid JSON" ;;
    22) fail "release.json has an unsupported format" ;;
    23) fail "release.json has an invalid lifecycle_protocol" ;;
    24) fail "release.json has an unsupported lifecycle protocol" ;;
    *) fail "release.json validation failed" ;;
  esac
}

add_parent_dirs() {
  local path="$1" prefix=""
  IFS=/ read -r -a parts <<<"$path"
  local i
  for ((i = 0; i < ${#parts[@]} - 1; i++)); do
    if [[ -z "$prefix" ]]; then prefix="${parts[$i]}"; else prefix="$prefix/${parts[$i]}"; fi
    local candidate="$prefix/" existing seen=false
    for existing in "${ALLOWED_DIRS[@]:-}"; do
      [[ "$existing" == "$candidate" ]] && seen=true
    done
    [[ "$seen" == true ]] || ALLOWED_DIRS+=("$candidate")
  done
}

main() {
  local archive="${EDGEZERO__APP__RELEASE__ARCHIVE:-}"
  local expected_digest="${EDGEZERO__APP__RELEASE__SHA256:-}"
  local expected_revision="${EDGEZERO__APP__RELEASE__EXPECTED_SOURCE_REVISION:-}"
  local release_root="${EDGEZERO__APP__RELEASE__ROOT:-}"
  require_input app-release-archive "$archive"
  require_input app-release-sha256 "$expected_digest"
  require_input expected-source-revision "$expected_revision"
  require_input application-release-root "$release_root"
  [[ "$expected_digest" =~ ^[0-9a-f]{64}$ ]] || fail "app-release-sha256 must be 64 lowercase hexadecimal characters"
  [[ "$expected_revision" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || fail "expected-source-revision must be 40 or 64 lowercase hexadecimal characters"
  [[ -f "$archive" && ! -L "$archive" ]] || fail "application release archive is missing or is not a regular file"
  require_cmd jq
  require_cmd python3
  require_cmd tar

  local actual_digest
  actual_digest=$(sha256_file "$archive")
  [[ "$actual_digest" == "$expected_digest" ]] || fail "application release archive digest mismatch"
  [[ ! -e "$release_root" ]] || fail "application release root already exists"
  mkdir -p "$(dirname -- "$release_root")"

  local scratch
  scratch=$(mktemp -d "$(dirname -- "$release_root")/.edgezero-release.XXXXXX")
  # shellcheck disable=SC2064 # expand the action-owned path while the local exists
  trap "rm -rf -- '$scratch'" EXIT

  local listing
  listing=$(tar -tzf "$archive") || fail "could not list application release archive"
  [[ -n "$listing" ]] || fail "application release archive is empty"
  if [[ -n "$(printf '%s\n' "$listing" | sort | uniq -d)" ]]; then
    fail "application release archive contains duplicate members"
  fi
  case "$listing" in
    *$'\r'* | *$'\t'*) fail "application release archive contains an invalid member name" ;;
  esac
  if printf '%s\n' "$listing" | grep -qE '(^/|(^|/)\.\.?(/|$)|\\|//)'; then
    fail "application release archive contains an unsafe member path"
  fi

  local release_json="$scratch/release.json"
  tar -xOzf "$archive" release.json >"$release_json" 2>/dev/null || fail "application release archive is missing release.json"
  validate_release_json_syntax "$release_json"

  assert_exact_keys "$release_json" 'type == "object" and (keys == ["adapter","app_cli","format","lifecycle_protocol","manifests","package","source_revision"])' root
  assert_exact_keys "$release_json" '.app_cli | type == "object" and (keys == ["path","sha256"])' app_cli
  assert_exact_keys "$release_json" '.package | type == "object" and (keys == ["path","sha256"])' package
  assert_exact_keys "$release_json" '.manifests | type == "object" and (keys == ["adapter","edgezero"])' manifests
  assert_exact_keys "$release_json" '.manifests.edgezero | type == "object" and (keys == ["path","sha256"])' manifests.edgezero
  assert_exact_keys "$release_json" '.manifests.adapter | type == "object" and (keys == ["path","sha256"])' manifests.adapter
  jq -e '.format == 1 and .lifecycle_protocol == 1 and .adapter == "fastly"' "$release_json" >/dev/null 2>&1 || fail "release.json has an unsupported format, lifecycle protocol, or adapter"

  local revision
  revision=$(jq -er '.source_revision | select(type == "string")' "$release_json") || fail "release.json has an invalid source_revision"
  [[ "$revision" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || fail "release.json source_revision must be 40 or 64 lowercase hexadecimal characters"
  [[ "$revision" == "$expected_revision" ]] || fail "release.json source_revision does not match expected-source-revision"

  local cli_path package_path edgezero_path adapter_path
  local cli_digest package_digest edgezero_digest adapter_digest
  cli_path=$(jq -er '.app_cli.path | select(type == "string")' "$release_json") || fail "release.json app_cli.path is invalid"
  package_path=$(jq -er '.package.path | select(type == "string")' "$release_json") || fail "release.json package.path is invalid"
  edgezero_path=$(jq -er '.manifests.edgezero.path | select(type == "string")' "$release_json") || fail "release.json manifests.edgezero.path is invalid"
  adapter_path=$(jq -er '.manifests.adapter.path | select(type == "string")' "$release_json") || fail "release.json manifests.adapter.path is invalid"
  cli_digest=$(jq -er '.app_cli.sha256 | select(type == "string")' "$release_json") || fail "release.json app_cli.sha256 is invalid"
  package_digest=$(jq -er '.package.sha256 | select(type == "string")' "$release_json") || fail "release.json package.sha256 is invalid"
  edgezero_digest=$(jq -er '.manifests.edgezero.sha256 | select(type == "string")' "$release_json") || fail "release.json manifests.edgezero.sha256 is invalid"
  adapter_digest=$(jq -er '.manifests.adapter.sha256 | select(type == "string")' "$release_json") || fail "release.json manifests.adapter.sha256 is invalid"

  validate_member_path "$cli_path" app_cli.path
  validate_member_path "$package_path" package.path
  validate_member_path "$edgezero_path" manifests.edgezero.path
  validate_member_path "$adapter_path" manifests.adapter.path
  local digest
  for digest in "$cli_digest" "$package_digest" "$edgezero_digest" "$adapter_digest"; do
    [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || fail "release.json contains an invalid sha256"
  done
  local unique_paths
  unique_paths=$(printf '%s\n' "$cli_path" "$package_path" "$edgezero_path" "$adapter_path" | sort -u | wc -l | tr -d ' ')
  [[ "$unique_paths" == 4 ]] || fail "release.json records duplicate member paths"

  local -a ALLOWED_FILES=(release.json "$cli_path" "$package_path" "$edgezero_path" "$adapter_path")
  local -a ALLOWED_DIRS=()
  add_parent_dirs "$cli_path"
  add_parent_dirs "$package_path"
  add_parent_dirs "$edgezero_path"
  add_parent_dirs "$adapter_path"
  local member verbose kind allowed
  while IFS= read -r member; do
    allowed="file"
    local candidate
    for candidate in "${ALLOWED_FILES[@]}"; do
      [[ "$candidate" == "$member" ]] && allowed=regular
    done
    if [[ "$allowed" == regular ]]; then
      verbose=$(tar -tvzf "$archive" -- "$member") || fail "could not inspect release member"
      [[ "$(printf '%s\n' "$verbose" | wc -l | tr -d ' ')" == 1 ]] || fail "release member is ambiguous"
      kind=${verbose:0:1}
      [[ "$kind" == "-" ]] || fail "release member '$member' is not a regular file"
      continue
    fi
    allowed=false
    for candidate in "${ALLOWED_DIRS[@]:-}"; do
      [[ "$candidate" == "$member" ]] && allowed=true
    done
    if [[ "$allowed" == true ]]; then
      verbose=$(tar -tvzf "$archive" -- "$member") || fail "could not inspect release directory"
      kind=${verbose:0:1}
      [[ "$kind" == "d" ]] || fail "release member '$member' is not a directory"
    else
      fail "application release archive contains unexpected member '$member'"
    fi
  done <<<"$listing"
  local expected_file
  for expected_file in release.json "$cli_path" "$package_path" "$edgezero_path" "$adapter_path"; do
    [[ $(printf '%s\n' "$listing" | grep -Fxc "$expected_file") == 1 ]] || fail "application release archive is missing '$expected_file'"
  done

  rm -f "$release_json"
  tar -xzf "$archive" -C "$scratch" || fail "could not extract application release archive"
  local scratch_real target_real
  scratch_real=$(canonical_path "$scratch")
  for expected_file in release.json "$cli_path" "$package_path" "$edgezero_path" "$adapter_path"; do
    [[ -f "$scratch/$expected_file" && ! -L "$scratch/$expected_file" ]] || fail "release member '$expected_file' is not a regular file"
    target_real=$(canonical_path "$scratch/$expected_file")
    is_under "$scratch_real" "$target_real" || fail "release member '$expected_file' escapes its root"
  done
  [[ "$(sha256_file "$scratch/$cli_path")" == "$cli_digest" ]] || fail "application CLI digest mismatch"
  [[ "$(sha256_file "$scratch/$package_path")" == "$package_digest" ]] || fail "Fastly package digest mismatch"
  [[ "$(sha256_file "$scratch/$edgezero_path")" == "$edgezero_digest" ]] || fail "edgezero manifest digest mismatch"
  [[ "$(sha256_file "$scratch/$adapter_path")" == "$adapter_digest" ]] || fail "Fastly manifest digest mismatch"

  mv "$scratch" "$release_root"
  trap - EXIT
  local root_real
  root_real=$(canonical_path "$release_root")
  notice "verified immutable Fastly application release"
  append_output release-root "$root_real"
  append_output app-cli-archive "$root_real/$cli_path"
  append_output application-manifest "$root_real/$edgezero_path"
  append_output adapter-manifest "$root_real/$adapter_path"
  append_output package-digest "$package_digest"
  append_output source-revision "$revision"
}

main "$@"
