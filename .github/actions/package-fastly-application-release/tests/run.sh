#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  printf 'package release test skipped outside Linux x86-64\n'
  exit 0
fi

ACTION_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/edgezero-package-release-test.XXXXXX")
trap 'rm -rf -- "$WORK_DIR"' EXIT
mkdir -p "$WORK_DIR/workspace/cli-root" "$WORK_DIR/runner"

cat >"$WORK_DIR/workspace/cli-root/app-cli" <<'CLI'
#!/usr/bin/env bash
case "$*" in
  --help) echo 'app-cli help' ;;
  'deploy --help') echo '--adapter --service-id --application-release --staging' ;;
  'config push --help') echo '--adapter --manifest --app-config --store --staging --no-env --yes --no-diff' ;;
  'healthcheck --help') echo '--adapter --service-id --version --domain --path --retry --retry-delay --timeout --staging' ;;
  'rollback --help') echo '--adapter --service-id --version --rollback-to --staging' ;;
  'active-version --help') echo '--adapter --service-id' ;;
  *) exit 2 ;;
esac
CLI
chmod +x "$WORK_DIR/workspace/cli-root/app-cli"
cat >"$WORK_DIR/workspace/cli-root/app-cli-meta.json" <<'JSON'
{"app-cli-bin":"app-cli","app-cli-version":"1.0.0","app-cli-package":"fixture-cli"}
JSON
tar -C "$WORK_DIR/workspace/cli-root" -cf "$WORK_DIR/workspace/app-cli.tar" \
  app-cli app-cli-meta.json
printf 'package\n' >"$WORK_DIR/workspace/app.tar.gz"
printf '[app]\nname = "fixture"\n[adapters.fastly.adapter]\nmanifest = "fastly.toml"\n' >"$WORK_DIR/workspace/edgezero.toml"
printf 'manifest_version = 3\nname = "fixture"\n' >"$WORK_DIR/workspace/fastly.toml"
: >"$WORK_DIR/output"

GITHUB_WORKSPACE="$WORK_DIR/workspace" \
  GITHUB_OUTPUT="$WORK_DIR/output" \
  RUNNER_TEMP="$WORK_DIR/runner" \
  EDGEZERO__RELEASE__APP_CLI_ARCHIVE=app-cli.tar \
  EDGEZERO__RELEASE__FASTLY_PACKAGE=app.tar.gz \
  EDGEZERO__RELEASE__APPLICATION_MANIFEST=edgezero.toml \
  EDGEZERO__RELEASE__ADAPTER_MANIFEST=fastly.toml \
  EDGEZERO__RELEASE__SOURCE_REVISION=0123456789abcdef0123456789abcdef01234567 \
  EDGEZERO__RELEASE__ARTIFACT_NAME=fixture-release \
  "$ACTION_DIR/scripts/package-release.sh" >/dev/null

archive=$(sed -n 's/^archive-path=//p' "$WORK_DIR/output")
[[ -f "$archive" ]]
tar -xOzf "$archive" release.json | jq -e \
  '.format == 1 and .lifecycle_protocol == 1 and .adapter == "fastly" and .manifests.adapter.path == "fastly.toml"' >/dev/null
tar -tzf "$archive" | grep -qx 'fastly.toml'
if tar -tzf "$archive" | grep -qx 'adapter/fastly.toml'; then
  printf 'packager relocated the declared Fastly manifest\n' >&2
  exit 1
fi
grep -qx 'artifact-name=fixture-release' "$WORK_DIR/output"
grep -Eq '^archive-sha256=[0-9a-f]{64}$' "$WORK_DIR/output"
grep -Eq '^package-sha256=[0-9a-f]{64}$' "$WORK_DIR/output"

perl -0pi -e 's/ --timeout//' "$WORK_DIR/workspace/cli-root/app-cli"
tar -C "$WORK_DIR/workspace/cli-root" -cf "$WORK_DIR/workspace/app-cli.tar" \
  app-cli app-cli-meta.json
if GITHUB_WORKSPACE="$WORK_DIR/workspace" \
  GITHUB_OUTPUT="$WORK_DIR/incompatible-output" \
  RUNNER_TEMP="$WORK_DIR/runner" \
  EDGEZERO__RELEASE__APP_CLI_ARCHIVE=app-cli.tar \
  EDGEZERO__RELEASE__FASTLY_PACKAGE=app.tar.gz \
  EDGEZERO__RELEASE__APPLICATION_MANIFEST=edgezero.toml \
  EDGEZERO__RELEASE__ADAPTER_MANIFEST=fastly.toml \
  EDGEZERO__RELEASE__SOURCE_REVISION=0123456789abcdef0123456789abcdef01234567 \
  EDGEZERO__RELEASE__ARTIFACT_NAME=incompatible-release \
  "$ACTION_DIR/scripts/package-release.sh" >"$WORK_DIR/incompatible.log" 2>&1; then
  printf 'packager accepted a CLI without the complete lifecycle protocol\n' >&2
  exit 1
fi
grep -Fq "healthcheck --help' lacks '--timeout'" "$WORK_DIR/incompatible.log"

printf 'Fastly application release packaging test passed\n'
