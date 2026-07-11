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

  mkdir -p "$app_dir/src" "$app_dir/bin"
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

  # Minimal Fastly manifest so the CLI's Fastly deploy path proceeds to invoke
  # the (fake) `fastly` binary instead of erroring on a missing manifest.
  cat >fastly.toml <<'FTOML'
manifest_version = 3
name = "fixture-app"
language = "rust"
FTOML

  # Fake `fastly` so `fastly compute deploy` records argv and reports success.
  cat >bin/fastly <<'FASTLY'
#!/usr/bin/env bash
printf '%s\n' "$@" >>"$GITHUB_WORKSPACE/fixture-app/fastly-argv.txt"
echo "SUCCESS: Deployed package (version 7)"
FASTLY
  chmod +x bin/fastly
  printf '%s\n' "$app_dir/bin" >>"${GITHUB_PATH:?GITHUB_PATH is required}"

  git add -A
  git commit -q -m fixture
}

main "$@"
