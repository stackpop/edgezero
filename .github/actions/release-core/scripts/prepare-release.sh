#!/usr/bin/env bash
set -euo pipefail

# Verifies and extracts an immutable application release before any provider
# operation. It validates the outer digest, exact archive members, member types,
# path confinement, recorded member digests, adapter/protocol identity, source
# revision, and the selected adapter-manifest reference in edgezero.toml.
#
# Reads (env):
#   EDGEZERO__APP__RELEASE__ARCHIVE                     required  release archive
#   EDGEZERO__APP__RELEASE__SHA256                      required  expected archive digest
#   EDGEZERO__APP__RELEASE__EXPECTED_SOURCE_REVISION    required  selected source revision
#   EDGEZERO__APP__RELEASE__EXPECTED_ADAPTER            required  wrapper-owned adapter identity
#   EDGEZERO__APP__RELEASE__EXPECTED_LIFECYCLE_PROTOCOL required  wrapper-owned protocol version
#   EDGEZERO__APP__RELEASE__ROOT                        required  new extraction root
# Writes (outputs):
#   release-root, app-cli-archive, application-manifest, adapter-manifest,
#   package, package-digest, source-revision

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

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
  local file="$1" expected_protocol="$2" format_tag format_value protocol_tag protocol_value
  jq -e '.' "$file" >/dev/null 2>&1 || fail "release.json is not valid JSON"
  yq -p json -o json -I=0 '.' "$file" >/dev/null 2>&1 ||
    fail "release.json is not valid JSON"
  # shellcheck disable=SC2016 # $keys is a yq variable, not a shell variable
  yq -p yaml -o json -I=0 '
    [.. | select(tag == "!!map")
      | (keys as $keys | ($keys | length) == ($keys | unique | length))]
    | all
  ' "$file" 2>/dev/null | grep -qx true ||
    fail "release.json contains a duplicate field"

  format_tag=$(yq -p yaml -o yaml -r '.format | tag' "$file" 2>/dev/null) ||
    fail "release.json has an unsupported format"
  format_value=$(yq -p yaml -o yaml -r '.format | to_string' "$file" 2>/dev/null) ||
    fail "release.json has an unsupported format"
  [[ "$format_tag" == '!!int' && "$format_value" == 1 ]] ||
    fail "release.json has an unsupported format"

  protocol_tag=$(yq -p yaml -o yaml -r '.lifecycle_protocol | tag' "$file" 2>/dev/null) ||
    fail "release.json has an invalid lifecycle_protocol"
  protocol_value=$(yq -p yaml -o yaml -r '.lifecycle_protocol | to_string' "$file" 2>/dev/null) ||
    fail "release.json has an invalid lifecycle_protocol"
  [[ "$protocol_tag" == '!!int' ]] ||
    fail "release.json has an invalid lifecycle_protocol"
  [[ "$protocol_value" == "$expected_protocol" ]] ||
    fail "release.json has an unsupported lifecycle protocol"
}

validate_manifest_reference() {
  local release_json="$1" application_manifest="$2" expected_adapter="$3" parsed status=0
  parsed="$release_json.application.json"
  if ! yq -p toml -o json -I=0 '.' "$application_manifest" >"$parsed" 2>/dev/null; then
    rm -f "$parsed"
    return 1
  fi
  jq -e --arg adapter "$expected_adapter" --slurpfile application "$parsed" '
    $application[0] as $app
    | ($app.adapters | type == "object")
      and ([$app.adapters | keys[] | ascii_downcase] as $names
        | ($names | length) == ($names | unique | length))
      and ([$app.adapters | to_entries[]
        | select((.key | ascii_downcase) == $adapter)] as $selected
        | ($selected | length) == 1
          and ($selected[0].value | type == "object")
          and ($selected[0].value.adapter | type == "object")
          and ($selected[0].value.adapter.manifest | type == "string" and length > 0)
          and ($selected[0].value.adapter.manifest == .manifests.adapter.path))
  ' "$release_json" >/dev/null 2>&1 || status=$?
  rm -f "$parsed"
  return "$status"
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
  local expected_adapter="${EDGEZERO__APP__RELEASE__EXPECTED_ADAPTER:-}"
  local expected_protocol="${EDGEZERO__APP__RELEASE__EXPECTED_LIFECYCLE_PROTOCOL:-}"
  local release_root="${EDGEZERO__APP__RELEASE__ROOT:-}"
  require_input app-release-archive "$archive"
  require_input app-release-sha256 "$expected_digest"
  require_input expected-source-revision "$expected_revision"
  require_input expected-adapter "$expected_adapter"
  require_input expected-lifecycle-protocol "$expected_protocol"
  require_input application-release-root "$release_root"
  [[ "$expected_digest" =~ ^[0-9a-f]{64}$ ]] || fail "app-release-sha256 must be 64 lowercase hexadecimal characters"
  [[ "$expected_revision" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || fail "expected-source-revision must be 40 or 64 lowercase hexadecimal characters"
  [[ "$expected_adapter" =~ ^[a-z][a-z0-9_-]*$ ]] || fail "expected-adapter is invalid"
  [[ "$expected_protocol" =~ ^[1-9][0-9]*$ ]] || fail "expected-lifecycle-protocol must be a positive integer"
  [[ -f "$archive" && ! -L "$archive" ]] || fail "application release archive is missing or is not a regular file"
  require_cmd jq
  require_cmd tar
  require_yq_v4

  [[ ! -e "$release_root" ]] || fail "application release root already exists"
  mkdir -p "$(dirname -- "$release_root")"

  local scratch
  scratch=$(mktemp -d "$(dirname -- "$release_root")/.edgezero-release.XXXXXX")
  # shellcheck disable=SC2064 # expand the action-owned path while the local exists
  trap "rm -rf -- '$scratch'" EXIT

  # Authenticate and inspect one action-owned copy. The caller's path is never
  # reopened after this copy, so replacing it cannot change bytes between the
  # digest check, archive inspection, and extraction.
  local trusted_archive="$scratch/app-release.tar.gz" actual_digest
  cp -- "$archive" "$trusted_archive"
  actual_digest=$(sha256_file "$trusted_archive")
  [[ "$actual_digest" == "$expected_digest" ]] || fail "application release archive digest mismatch"

  local listing
  listing=$(tar -tzf "$trusted_archive") || fail "could not list application release archive"
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
  tar -xOzf "$trusted_archive" release.json >"$release_json" 2>/dev/null || fail "application release archive is missing release.json"
  validate_release_json_syntax "$release_json" "$expected_protocol"

  assert_exact_keys "$release_json" 'type == "object" and (keys == ["adapter","app_cli","format","lifecycle_protocol","manifests","package","source_revision"])' root
  assert_exact_keys "$release_json" '.app_cli | type == "object" and (keys == ["path","sha256"])' app_cli
  assert_exact_keys "$release_json" '.package | type == "object" and (keys == ["path","sha256"])' package
  assert_exact_keys "$release_json" '.manifests | type == "object" and (keys == ["adapter","edgezero"])' manifests
  assert_exact_keys "$release_json" '.manifests.edgezero | type == "object" and (keys == ["path","sha256"])' manifests.edgezero
  assert_exact_keys "$release_json" '.manifests.adapter | type == "object" and (keys == ["path","sha256"])' manifests.adapter
  jq -e --arg adapter "$expected_adapter" --argjson protocol "$expected_protocol" \
    '.format == 1 and .lifecycle_protocol == $protocol and .adapter == $adapter' \
    "$release_json" >/dev/null 2>&1 ||
    fail "release.json has an unsupported format, lifecycle protocol, or adapter"

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
      verbose=$(tar -tvzf "$trusted_archive" -- "$member") || fail "could not inspect release member"
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
      verbose=$(tar -tvzf "$trusted_archive" -- "$member") || fail "could not inspect release directory"
      kind=${verbose:0:1}
      [[ "$kind" == "d" ]] || fail "release member '$member' is not a directory"
    else
      fail "application release archive contains unexpected member '$member'"
    fi
  done <<<"$listing"
  local expected_file
  for expected_file in "${ALLOWED_FILES[@]}"; do
    [[ $(printf '%s\n' "$listing" | grep -Fxc "$expected_file") == 1 ]] || fail "application release archive is missing '$expected_file'"
  done

  rm -f "$release_json"
  tar -xzf "$trusted_archive" -C "$scratch" || fail "could not extract application release archive"
  local scratch_real target_real
  scratch_real=$(canonical_path "$scratch")
  for expected_file in "${ALLOWED_FILES[@]}"; do
    [[ -f "$scratch/$expected_file" && ! -L "$scratch/$expected_file" ]] || fail "release member '$expected_file' is not a regular file"
    target_real=$(canonical_path "$scratch/$expected_file")
    is_under "$scratch_real" "$target_real" || fail "release member '$expected_file' escapes its root"
  done
  [[ "$(sha256_file "$scratch/$cli_path")" == "$cli_digest" ]] || fail "application CLI digest mismatch"
  [[ "$(sha256_file "$scratch/$package_path")" == "$package_digest" ]] || fail "adapter package digest mismatch"
  [[ "$(sha256_file "$scratch/$edgezero_path")" == "$edgezero_digest" ]] || fail "edgezero manifest digest mismatch"
  [[ "$(sha256_file "$scratch/$adapter_path")" == "$adapter_digest" ]] || fail "adapter manifest digest mismatch"
  validate_manifest_reference "$scratch/release.json" "$scratch/$edgezero_path" "$expected_adapter" ||
    fail "release.json adapter manifest does not match edgezero.toml"

  rm -f "$trusted_archive"
  mv "$scratch" "$release_root"
  trap - EXIT
  local root_real
  root_real=$(canonical_path "$release_root")
  notice "verified immutable application release for adapter '$expected_adapter'"
  append_output release-root "$root_real"
  append_output app-cli-archive "$root_real/$cli_path"
  append_output application-manifest "$root_real/$edgezero_path"
  append_output adapter-manifest "$root_real/$adapter_path"
  append_output package "$root_real/$package_path"
  append_output package-digest "$package_digest"
  append_output source-revision "$revision"
}

main "$@"
