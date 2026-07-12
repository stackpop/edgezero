#!/usr/bin/env bash
set -euo pipefail

# Creates the minimal fixture application the composite smoke test deploys:
# a standalone Cargo package (kept out of the surrounding edgezero workspace),
# a committed clean tree, and an edgezero.toml whose Fastly deploy command is
# overridden by a marker script. The script emits `version=<N>` (so version
# threading is exercised), records what credentials it actually saw (so the
# provider-env boundary is exercised), and records its argv — all without
# contacting Fastly.
#
# Inputs (environment): GITHUB_WORKSPACE (required).

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local app_dir="$workspace/fixture-app"

  mkdir -p "$app_dir/src"
  cd "$app_dir"

  git init -q
  git config user.email test@example.com
  git config user.name Test

  cat >Cargo.toml <<'TOML'
[package]
name = "fixture-app"
version = "0.0.0"
edition = "2021"

# Standalone: keep the fixture out of the surrounding edgezero workspace.
[workspace]
TOML

  echo 'fn main() {}' >src/main.rs
  cargo generate-lockfile

  # Marker "deploy" script the CLI runs in place of `fastly compute deploy`.
  # It records the credentials it actually saw and its argv, and emits a version
  # line so `deploy-fastly` can thread `fastly-version`.
  cat >fake-deploy.sh <<'SH'
#!/usr/bin/env bash
{
  printf 'token=%s\n' "${FASTLY_API_TOKEN:-MISSING}"
  printf 'service-id=%s\n' "${FASTLY_SERVICE_ID:-MISSING}"
  # Boundary check: an inherited endpoint alias must have been cleared.
  printf 'endpoint=%s\n' "${FASTLY_ENDPOINT:-CLEARED}"
} >"${GITHUB_WORKSPACE}/fixture-app/env-seen.txt"
printf '%s\n' "$@" >"${GITHUB_WORKSPACE}/fixture-app/deploy-argv.txt"
echo "version=7"
SH
  chmod +x fake-deploy.sh

  # Override the Fastly deploy command so `edgezero deploy --adapter fastly` runs
  # the marker script instead of the real `fastly compute deploy` (which would
  # need a built package and live credentials).
  cat >edgezero.toml <<'ETOML'
[adapters.fastly.commands]
deploy = "bash fake-deploy.sh"
ETOML

  git add -A
  git commit -q -m fixture
}

main "$@"
