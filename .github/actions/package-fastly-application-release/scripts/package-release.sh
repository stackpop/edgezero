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

resolve_adapter_manifest_member() {
  local application_manifest="$1" adapter_manifest="$2"
  python3 - "$application_manifest" "$adapter_manifest" <<'PY'
import pathlib
import sys

try:
    import tomllib
except ImportError:
    sys.stderr.write("Python 3.11 or newer is required to parse edgezero.toml\n")
    sys.exit(20)

application = pathlib.Path(sys.argv[1]).resolve(strict=True)
adapter = pathlib.Path(sys.argv[2]).resolve(strict=True)
try:
    with application.open("rb") as source:
        document = tomllib.load(source)
except (OSError, tomllib.TOMLDecodeError) as error:
    sys.stderr.write(f"could not parse application-manifest: {error}\n")
    sys.exit(21)

adapters = document.get("adapters")
if not isinstance(adapters, dict):
    sys.stderr.write("application-manifest must declare [adapters.fastly.adapter]\n")
    sys.exit(22)
matches = [value for key, value in adapters.items() if key.lower() == "fastly"]
if len(matches) != 1 or not isinstance(matches[0], dict):
    sys.stderr.write("application-manifest must declare exactly one Fastly adapter\n")
    sys.exit(23)
adapter_config = matches[0].get("adapter")
relative = adapter_config.get("manifest") if isinstance(adapter_config, dict) else None
if not isinstance(relative, str) or not relative:
    sys.stderr.write("application-manifest Fastly adapter must declare a manifest path\n")
    sys.exit(24)
path = pathlib.PurePosixPath(relative)
if "\\" in relative or path.is_absolute() or path.as_posix() != relative or any(part in ("", ".", "..") for part in path.parts):
    sys.stderr.write("application-manifest Fastly manifest path must be normalized and relative\n")
    sys.exit(25)
try:
    referenced = (application.parent / pathlib.Path(*path.parts)).resolve(strict=True)
except OSError as error:
    sys.stderr.write(f"application-manifest Fastly manifest path cannot be resolved: {error}\n")
    sys.exit(26)
if not referenced.is_file() or referenced != adapter:
    sys.stderr.write("adapter-manifest must be the file referenced by application-manifest\n")
    sys.exit(27)
if relative in {"release.json", "edgezero.toml", "cli/app-cli.tar", "package/app.tar.gz"}:
    sys.stderr.write("application-manifest Fastly manifest path collides with a reserved release member\n")
    sys.exit(28)
print(relative)
PY
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
  probe_flags "$cli_path" deploy --adapter --service-id --application-release --staging
  probe_flags "$cli_path" 'config push' --adapter --manifest --app-config --store --staging --no-env --yes --no-diff
  probe_flags "$cli_path" healthcheck --adapter --service-id --version --domain --path --retry --retry-delay --timeout --staging
  probe_flags "$cli_path" rollback --adapter --service-id --version --rollback-to --staging
  probe_flags "$cli_path" active-version --adapter --service-id

  local adapter_member stage release_archive package_digest archive_digest verify_root
  adapter_member=$(resolve_adapter_manifest_member "$application_manifest" "$adapter_manifest") ||
    fail "could not resolve the Fastly manifest path declared by application-manifest"
  stage="$work/stage"
  mkdir -p "$stage/cli" "$stage/package" "$(dirname -- "$stage/$adapter_member")"
  cp "$cli_archive" "$stage/cli/app-cli.tar"
  cp "$package" "$stage/package/app.tar.gz"
  cp "$application_manifest" "$stage/edgezero.toml"
  cp "$adapter_manifest" "$stage/$adapter_member"
  package_digest=$(sha256_file "$stage/package/app.tar.gz")
  jq -n \
    --arg revision "$revision" \
    --arg cli "$(sha256_file "$stage/cli/app-cli.tar")" \
    --arg package "$package_digest" \
    --arg edgezero "$(sha256_file "$stage/edgezero.toml")" \
    --arg adapter "$(sha256_file "$stage/$adapter_member")" \
    --arg adapter_path "$adapter_member" \
    '{format:1,lifecycle_protocol:1,source_revision:$revision,adapter:"fastly",app_cli:{path:"cli/app-cli.tar",sha256:$cli},package:{path:"package/app.tar.gz",sha256:$package},manifests:{edgezero:{path:"edgezero.toml",sha256:$edgezero},adapter:{path:$adapter_path,sha256:$adapter}}}' \
    >"$stage/release.json"
  release_archive="$work/app-release.tar.gz"
  tar -C "$stage" -czf "$release_archive" \
    release.json cli/app-cli.tar package/app.tar.gz edgezero.toml "$adapter_member"
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
