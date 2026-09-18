#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

confined_file() {
  local raw="$1" label="$2" workspace_real="$3" candidate
  [[ -n "$raw" ]] || fail "$label is required"
  case "$raw" in
    /*) candidate="$raw" ;;
    *) candidate="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}/$raw" ;;
  esac
  [[ -f "$candidate" && ! -L "$candidate" ]] || fail "$label must be a regular file"
  candidate=$(canonical_path "$candidate")
  is_under "$workspace_real" "$candidate" || fail "$label must resolve beneath github.workspace"
  printf '%s\n' "$candidate"
}

probe_flags() {
  local cli="$1" command="$2"
  shift 2
  local help expected
  case "$command" in
    'config push')
      help=$(env -i PATH="/usr/bin:/bin" HOME="${HOME:-/tmp}" "$cli" config push --help 2>&1) ||
        fail "application CLI does not support 'config push --help' required by lifecycle protocol 1"
      ;;
    deploy | healthcheck | rollback | active-version)
      help=$(env -i PATH="/usr/bin:/bin" HOME="${HOME:-/tmp}" "$cli" "$command" --help 2>&1) ||
        fail "application CLI does not support '$command --help' required by lifecycle protocol 1"
      ;;
    *) fail "internal error: unsupported lifecycle command probe '$command'" ;;
  esac
  for expected in "$@"; do
    awk -v flag="$expected" '
      {
        for (field = 1; field <= NF; field++) {
          if ($field == flag || index($field, flag "=") == 1) found = 1
        }
      }
      END { exit !found }
    ' <<<"$help" ||
      fail "application CLI '$command --help' lacks '$expected' required by lifecycle protocol 1"
  done
}

package_adapter_manifests() {
  local application_manifest="$1" adapter_manifest="$2" stage="$3" output="$4"
  local parsed records entries application_root metadata_root
  metadata_root=$(dirname -- "$output")
  parsed="$metadata_root/application-manifest.json"
  records="$metadata_root/adapter-manifest-records.json"
  entries="$metadata_root/adapter-manifest-entries.jsonl"
  yq -p toml -o json -I=0 '.' "$application_manifest" >"$parsed" 2>/dev/null ||
    fail "could not parse application-manifest as TOML"
  jq -e '
    (.adapters | type == "object")
    and (.adapters | to_entries | length > 0)
    and (.adapters | to_entries | all(.[].key;
      length > 0 and (explode | all(.[]; . >= 32 and . != 127))))
    and ([.adapters | keys[] | ascii_downcase] as $names
      | ($names | length) == ($names | unique | length))
    and (.adapters | to_entries | all(.[].value;
      type == "object"
      and ((has("adapter") | not) or (.adapter | type == "object"))
      and ((has("adapter") | not) or (.adapter | has("manifest") | not)
        or (.adapter.manifest | type == "string" and length > 0))))
    and ([.adapters | to_entries[] | select((.key | ascii_downcase) == "fastly")]
      | length == 1 and (.[0].value.adapter.manifest | type == "string" and length > 0))
  ' "$parsed" >/dev/null 2>&1 ||
    fail "application-manifest must declare one valid [adapters.fastly.adapter] manifest"
  jq -c '
    [.adapters | to_entries[]
      | select(.value.adapter.manifest? != null)
      | {name: .key, path: .value.adapter.manifest}]
    | sort_by([(.name | ascii_downcase), .name])
  ' "$parsed" >"$records"

  application_root=$(canonical_path "$(dirname -- "$application_manifest")")
  : >"$entries"
  local name relative candidate referenced destination digest folded fastly_manifest=""
  while IFS= read -r -d '' name && IFS= read -r -d '' relative; do
    [[ -n "$relative" && "$relative" =~ ^[A-Za-z0-9._/-]+$ ]] ||
      fail "application-manifest adapter '$name' has an invalid manifest path"
    case "$relative" in
      /* | *\\* | */ | ./* | *//* | release.json | edgezero.toml | cli/app-cli.tar | package/app.tar.gz)
        fail "application-manifest adapter '$name' manifest path must be normalized, relative, and distinct from reserved release members"
        ;;
    esac
    local part
    local -a parts
    IFS=/ read -r -a parts <<<"$relative"
    for part in "${parts[@]}"; do
      [[ -n "$part" && "$part" != . && "$part" != .. ]] ||
        fail "application-manifest adapter '$name' manifest path must be normalized and relative"
    done
    candidate="$application_root/$relative"
    [[ ! -L "$candidate" ]] ||
      fail "application-manifest adapter '$name' manifest must not be a symlink"
    [[ -f "$candidate" ]] ||
      fail "application-manifest adapter '$name' manifest is not a regular file"
    referenced=$(canonical_path "$candidate")
    is_under "$application_root" "$referenced" ||
      fail "application-manifest adapter '$name' manifest must resolve beneath the application root"
    destination="$stage/$relative"
    mkdir -p "$(dirname -- "$destination")"
    cp "$referenced" "$destination"
    digest=$(sha256_file "$destination")
    jq -cn --arg name "$name" --arg path "$relative" --arg sha256 "$digest" \
      '{name:$name,path:$path,sha256:$sha256}' >>"$entries"
    folded=$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')
    [[ "$folded" != fastly ]] || fastly_manifest="$referenced"
  done < <(jq -j '.[] | (.name, "\u0000", .path, "\u0000")' "$records")

  [[ -n "$fastly_manifest" ]] || fail "application-manifest records no Fastly manifest"
  [[ "$fastly_manifest" == "$adapter_manifest" ]] ||
    fail "adapter-manifest must be the Fastly file referenced by application-manifest"
  jq -sc '.' "$entries" >"$output"
  rm -f "$parsed" "$records" "$entries"
}

main() {
  local workspace_real runner_temp artifact_name revision
  workspace_real=$(canonical_path "${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}")
  runner_temp="${RUNNER_TEMP:-/tmp}"
  artifact_name="${EDGEZERO__RELEASE__ARTIFACT_NAME:-application-release}"
  revision="${EDGEZERO__RELEASE__SOURCE_REVISION:-}"
  [[ "$artifact_name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]] ||
    fail "artifact-name contains unsupported characters"
  [[ "$revision" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] ||
    fail "source-revision must be 40 or 64 lowercase hexadecimal characters"
  for command in jq tar; do require_cmd "$command"; done
  require_yq_v4

  local cli_archive package application_manifest adapter_manifest
  cli_archive=$(confined_file "${EDGEZERO__RELEASE__APP_CLI_ARCHIVE:-}" app-cli-archive "$workspace_real")
  package=$(confined_file "${EDGEZERO__RELEASE__FASTLY_PACKAGE:-}" fastly-package "$workspace_real")
  application_manifest=$(confined_file "${EDGEZERO__RELEASE__APPLICATION_MANIFEST:-}" application-manifest "$workspace_real")
  adapter_manifest=$(confined_file "${EDGEZERO__RELEASE__ADAPTER_MANIFEST:-}" adapter-manifest "$workspace_real")

  local work cli_outputs cli_path
  work=$(mktemp -d "$runner_temp/edgezero-package-release.XXXXXX")
  trap 'rm -rf -- "$work"' EXIT
  cli_outputs="$work/cli.outputs"
  : >"$cli_outputs"
  EDGEZERO__APP__CLI__ARCHIVE="$cli_archive" \
    EDGEZERO__ACTION__TOOL_ROOT="$work/tool" \
    GITHUB_OUTPUT="$cli_outputs" \
    "$SCRIPT_DIR/../../deploy-core/scripts/download-app-cli.sh" >/dev/null
  cli_path=$(sed -n 's/^app-cli-path=//p' "$cli_outputs")
  [[ -n "$cli_path" && -x "$cli_path" ]] || fail "application CLI verification emitted no executable path"
  probe_flags "$cli_path" deploy --adapter --service-id --application-release --staging
  probe_flags "$cli_path" 'config push' --adapter --manifest --app-config --store --staging --no-env --yes --no-diff
  probe_flags "$cli_path" healthcheck --adapter --service-id --version --domain --path --retry --retry-delay --timeout --staging
  probe_flags "$cli_path" rollback --adapter --service-id --version --rollback-to --staging
  probe_flags "$cli_path" active-version --adapter --service-id

  local stage adapter_metadata release_archive package_digest archive_digest verify_root
  stage="$work/stage"
  mkdir -p "$stage/cli" "$stage/package"
  cp "$cli_archive" "$stage/cli/app-cli.tar"
  cp "$package" "$stage/package/app.tar.gz"
  cp "$application_manifest" "$stage/edgezero.toml"
  adapter_metadata="$work/adapter-manifests.json"
  package_adapter_manifests "$application_manifest" "$adapter_manifest" "$stage" "$adapter_metadata" ||
    fail "could not package every adapter manifest referenced by application-manifest"
  package_digest=$(sha256_file "$stage/package/app.tar.gz")
  jq -n \
    --arg revision "$revision" \
    --arg cli "$(sha256_file "$stage/cli/app-cli.tar")" \
    --arg package "$package_digest" \
    --arg edgezero "$(sha256_file "$stage/edgezero.toml")" \
    --slurpfile adapters "$adapter_metadata" \
    '{format:1,lifecycle_protocol:1,source_revision:$revision,adapter:"fastly",app_cli:{path:"cli/app-cli.tar",sha256:$cli},package:{path:"package/app.tar.gz",sha256:$package},manifests:{edgezero:{path:"edgezero.toml",sha256:$edgezero},adapters:$adapters[0]}}' \
    >"$stage/release.json"
  release_archive="$work/app-release.tar.gz"
  local -a release_members=(release.json cli/app-cli.tar package/app.tar.gz edgezero.toml)
  local member existing seen
  while IFS= read -r member; do
    seen=false
    for existing in "${release_members[@]}"; do
      [[ "$existing" == "$member" ]] && seen=true
    done
    [[ "$seen" == true ]] || release_members+=("$member")
  done < <(jq -r '.[].path' "$adapter_metadata")
  tar -C "$stage" -czf "$release_archive" "${release_members[@]}"
  archive_digest=$(sha256_file "$release_archive")

  verify_root="$work/verified"
  EDGEZERO__APP__RELEASE__ARCHIVE="$release_archive" \
    EDGEZERO__APP__RELEASE__SHA256="$archive_digest" \
    EDGEZERO__APP__RELEASE__EXPECTED_SOURCE_REVISION="$revision" \
    EDGEZERO__APP__RELEASE__ROOT="$verify_root" \
    GITHUB_OUTPUT="$work/verify.outputs" \
    "$SCRIPT_DIR/../../fastly-common/scripts/prepare-release.sh" >/dev/null

  append_output artifact-name "$artifact_name"
  append_output archive-path "$release_archive"
  append_output workspace-path "$work"
  append_output archive-sha256 "$archive_digest"
  append_output package-sha256 "$package_digest"
  append_output source-revision "$revision"
  trap - EXIT
}

main "$@"
