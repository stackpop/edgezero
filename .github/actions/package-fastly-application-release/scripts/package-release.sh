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

probe_flag() {
  local cli="$1" expected="$2"
  shift 2
  local help
  help=$(env -i PATH="/usr/bin:/bin" HOME="${HOME:-/tmp}" "$cli" "$@" --help 2>&1) ||
    fail "application CLI does not support '$* --help' required by lifecycle protocol 1"
  grep -Fq -- "$expected" <<<"$help" ||
    fail "application CLI '$* --help' lacks '$expected' required by lifecycle protocol 1"
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
  for command in jq python3 tar; do require_cmd "$command"; done

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
  probe_flag "$cli_path" --application-release deploy
  probe_flag "$cli_path" --staging config push
  probe_flag "$cli_path" --domain healthcheck
  probe_flag "$cli_path" --rollback-to rollback

  local stage="$work/stage" release_archive package_digest archive_digest verify_root
  mkdir -p "$stage/cli" "$stage/package" "$stage/adapter"
  cp "$cli_archive" "$stage/cli/app-cli.tar"
  cp "$package" "$stage/package/app.tar.gz"
  cp "$application_manifest" "$stage/edgezero.toml"
  cp "$adapter_manifest" "$stage/adapter/fastly.toml"
  package_digest=$(sha256_file "$stage/package/app.tar.gz")
  jq -n \
    --arg revision "$revision" \
    --arg cli "$(sha256_file "$stage/cli/app-cli.tar")" \
    --arg package "$package_digest" \
    --arg edgezero "$(sha256_file "$stage/edgezero.toml")" \
    --arg adapter "$(sha256_file "$stage/adapter/fastly.toml")" \
    '{format:1,lifecycle_protocol:1,source_revision:$revision,adapter:"fastly",app_cli:{path:"cli/app-cli.tar",sha256:$cli},package:{path:"package/app.tar.gz",sha256:$package},manifests:{edgezero:{path:"edgezero.toml",sha256:$edgezero},adapter:{path:"adapter/fastly.toml",sha256:$adapter}}}' \
    >"$stage/release.json"
  release_archive="$work/app-release.tar.gz"
  tar -C "$stage" -czf "$release_archive" \
    release.json cli/app-cli.tar package/app.tar.gz edgezero.toml adapter/fastly.toml
  archive_digest=$(sha256_file "$release_archive")

  verify_root="$work/verified"
  EDGEZERO__APP__RELEASE__ARCHIVE="$release_archive" \
    EDGEZERO__APP__RELEASE__SHA256="$archive_digest" \
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
