#!/usr/bin/env bash
set -euo pipefail

TEST_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ACTION_DIR=$(cd -- "$TEST_DIR/.." && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$ACTION_DIR/../deploy-core/scripts/common.sh"

work=$(mktemp -d "${TMPDIR:-/tmp}/edgezero-release-core-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
stage="$work/stage"
mkdir -p "$stage/cli" "$stage/package" "$stage/adapters"
printf 'cli\n' >"$stage/cli/app-cli.tar"
printf 'package\n' >"$stage/package/app.tar.gz"
cat >"$stage/edgezero.toml" <<'TOML'
[app]
name = "fixture"

[adapters.synthetic.adapter]
manifest = "adapters/synthetic.toml"

[adapters.unselected.adapter]
manifest = "adapters/unselected.toml"
TOML
printf 'name = "synthetic"\n' >"$stage/adapters/synthetic.toml"
revision=0123456789abcdef0123456789abcdef01234567
jq -n \
  --arg revision "$revision" \
  --arg cli "$(sha256_file "$stage/cli/app-cli.tar")" \
  --arg package "$(sha256_file "$stage/package/app.tar.gz")" \
  --arg edgezero "$(sha256_file "$stage/edgezero.toml")" \
  --arg adapter "$(sha256_file "$stage/adapters/synthetic.toml")" \
  '{format:1,lifecycle_protocol:7,source_revision:$revision,adapter:"synthetic",app_cli:{path:"cli/app-cli.tar",sha256:$cli},package:{path:"package/app.tar.gz",sha256:$package},manifests:{edgezero:{path:"edgezero.toml",sha256:$edgezero},adapter:{path:"adapters/synthetic.toml",sha256:$adapter}}}' \
  >"$stage/release.json"
archive="$work/application-release.tar.gz"
tar -C "$stage" -czf "$archive" \
  release.json cli/app-cli.tar package/app.tar.gz edgezero.toml adapters/synthetic.toml
digest=$(sha256_file "$archive")

verify() {
  local root="$1" adapter="$2" protocol="$3" output="$4"
  EDGEZERO__APP__RELEASE__ARCHIVE="$archive" \
    EDGEZERO__APP__RELEASE__SHA256="$digest" \
    EDGEZERO__APP__RELEASE__EXPECTED_SOURCE_REVISION="$revision" \
    EDGEZERO__APP__RELEASE__EXPECTED_ADAPTER="$adapter" \
    EDGEZERO__APP__RELEASE__EXPECTED_LIFECYCLE_PROTOCOL="$protocol" \
    EDGEZERO__APP__RELEASE__ROOT="$root" \
    GITHUB_OUTPUT="$output" \
    "$ACTION_DIR/scripts/prepare-release.sh"
}

output="$work/output"
verify "$work/release" synthetic 7 "$output" >/dev/null
root_real=$(canonical_path "$work/release")
grep -qx "release-root=$root_real" "$output"
grep -qx "app-cli-archive=$root_real/cli/app-cli.tar" "$output"
grep -qx "application-manifest=$root_real/edgezero.toml" "$output"
grep -qx "adapter-manifest=$root_real/adapters/synthetic.toml" "$output"
grep -qx "package=$root_real/package/app.tar.gz" "$output"
grep -qx "source-revision=$revision" "$output"

if verify "$work/wrong-adapter" other 7 "$work/wrong-adapter.out" >/dev/null 2>&1; then
  fail "release verifier accepted an adapter mismatch"
fi
if verify "$work/wrong-protocol" synthetic 8 "$work/wrong-protocol.out" >/dev/null 2>&1; then
  fail "release verifier accepted a lifecycle protocol mismatch"
fi

if [[ "$(uname -s)" == Linux && "$(uname -m)" == x86_64 ]]; then
  workspace="$work/package-workspace"
  policy_root="$work/policy"
  mkdir -p "$workspace/cli" "$workspace/adapters" "$policy_root" "$work/runner"
  cat >"$workspace/cli/app-cli" <<'CLI'
#!/usr/bin/env bash
case "$*" in
  --help) printf 'synthetic application CLI\n' ;;
  'publish --help') printf '%s\n' '--adapter --release' ;;
  *) exit 2 ;;
esac
CLI
  chmod +x "$workspace/cli/app-cli"
  printf '%s\n' '{"app-cli-bin":"app-cli","app-cli-version":"1.0.0","app-cli-package":"fixture"}' \
    >"$workspace/cli/app-cli-meta.json"
  tar -C "$workspace/cli" -cf "$workspace/app-cli.tar" app-cli app-cli-meta.json
  printf 'synthetic package\n' >"$workspace/package.tar.gz"
  cat >"$workspace/edgezero.toml" <<'TOML'
[app]
name = "fixture"

[adapters.synthetic.adapter]
manifest = "adapters/synthetic.toml"

[adapters.unselected.adapter]
manifest = "adapters/unselected.toml"
TOML
  printf 'name = "synthetic"\n' >"$workspace/adapters/synthetic.toml"
  printf 'name = "unselected"\n' >"$workspace/adapters/unselected.toml"
  cat >"$policy_root/lifecycle-protocol.json" <<'JSON'
{
  "lifecycle_protocol": 7,
  "probes": [
    {"command": ["publish"], "required_flags": ["--adapter", "--release"]}
  ]
}
JSON
  package_output="$work/package-output"
  GITHUB_WORKSPACE="$workspace" GITHUB_OUTPUT="$package_output" RUNNER_TEMP="$work/runner" \
    EDGEZERO__RELEASE__ADAPTER=synthetic \
    EDGEZERO__RELEASE__LIFECYCLE_CAPABILITIES="$policy_root/lifecycle-protocol.json" \
    EDGEZERO__RELEASE__POLICY_ROOT="$policy_root" \
    EDGEZERO__RELEASE__APP_CLI_ARCHIVE=app-cli.tar \
    EDGEZERO__RELEASE__PACKAGE=package.tar.gz \
    EDGEZERO__RELEASE__APPLICATION_MANIFEST=edgezero.toml \
    EDGEZERO__RELEASE__ADAPTER_MANIFEST=adapters/synthetic.toml \
    EDGEZERO__RELEASE__SOURCE_REVISION="$revision" \
    EDGEZERO__RELEASE__ARTIFACT_NAME=synthetic-release \
    "$ACTION_DIR/scripts/package-release.sh" >/dev/null
  packaged_archive=$(sed -n 's/^archive-path=//p' "$package_output")
  tar -xOzf "$packaged_archive" release.json | jq -e \
    '.adapter == "synthetic" and .lifecycle_protocol == 7
      and .manifests.adapter.path == "adapters/synthetic.toml"' >/dev/null
  tar -tzf "$packaged_archive" | grep -qx 'adapters/synthetic.toml'
  if tar -tzf "$packaged_archive" | grep -qx 'adapters/unselected.toml'; then
    fail "provider-neutral packager included an unselected adapter manifest"
  fi
fi

printf 'provider-neutral release contract tests passed\n'
