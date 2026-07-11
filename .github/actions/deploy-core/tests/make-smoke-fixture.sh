#!/usr/bin/env bash
set -euo pipefail

# Creates the minimal fixture application the composite smoke test deploys:
# a standalone Cargo package (kept out of the surrounding edgezero workspace),
# a committed clean tree, and a fake `fastly` binary on PATH that records its
# argv instead of contacting Fastly.
#
# Inputs (environment): GITHUB_WORKSPACE (required), GITHUB_PATH (required).

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

  # Override the Fastly deploy command so `edgezero deploy --adapter fastly`
  # runs a marker command (recording the passthrough argv) instead of the real
  # `fastly compute deploy`, which would require a built package and live creds.
  cat >edgezero.toml <<'ETOML'
[adapters.fastly.commands]
deploy = "printf '%s\n' > deploy-argv.txt"
ETOML

  git add -A
  git commit -q -m fixture
}

main "$@"
