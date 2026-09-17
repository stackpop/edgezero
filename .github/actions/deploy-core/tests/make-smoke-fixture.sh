#!/usr/bin/env bash
set -euo pipefail

# Builds the application-owned CLI source fixture, then packages that CLI with
# immutable Fastly package and manifest bytes into one verified application
# release. Runtime publisher/environment choices are deliberately absent here.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

write_source_fixture() {
  local mode="$1"
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local app_dir="$workspace/fixture-app"

  case "$mode" in
    store-free | store-aware) ;;
    *) fail "fixture mode must be store-free or store-aware" ;;
  esac

  mkdir -p "$app_dir/crates/fixture-app-cli/src"
  cd "$app_dir"

  git init -q
  git config user.email test@example.com
  git config user.name Test

  cat >Cargo.toml <<'TOML'
[workspace]
members = ["crates/fixture-app-cli"]
resolver = "2"
TOML

  cat >crates/fixture-app-cli/Cargo.toml <<'TOML'
[package]
name = "fixture-app-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "fixture-app-cli"
path = "src/main.rs"

[dependencies]
edgezero-cli = { path = "../../../crates/edgezero-cli", default-features = false, features = [
  "cli",
  "edgezero-adapter-fastly",
] }
clap = { version = "4", features = ["derive"] }
edgezero-core = { path = "../../../crates/edgezero-core" }
serde = { version = "1", features = ["derive"] }
validator = { version = "0.20", features = ["derive"] }
TOML

  cat >crates/fixture-app-cli/src/main.rs <<'RS'
use clap::{Parser, Subcommand};
use edgezero_cli::args::{
    ActiveVersionArgs, BuildArgs, ConfigPushArgs, DeployArgs, HealthcheckArgs, RollbackArgs,
};
use serde::{Deserialize, Serialize};
use validator::Validate;

#[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
struct FixtureAppConfig {
    greeting: String,
}

#[derive(Parser, Debug)]
#[command(name = "fixture-app-cli", version, about = "fixture app edge CLI")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    #[command(subcommand)]
    Config(ConfigCmd),
    Build(BuildArgs),
    Deploy(DeployArgs),
    Healthcheck(HealthcheckArgs),
    ActiveVersion(ActiveVersionArgs),
    Rollback(RollbackArgs),
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    Push(ConfigPushArgs),
}

fn main() {
    edgezero_cli::init_cli_logger();
    let result = match Args::parse().cmd {
        Cmd::Config(ConfigCmd::Push(args)) => {
            edgezero_cli::run_config_push_typed::<FixtureAppConfig>(&args)
        }
        Cmd::Build(args) => edgezero_cli::run_build(&args),
        Cmd::Deploy(args) => edgezero_cli::run_deploy(&args),
        Cmd::Healthcheck(args) => edgezero_cli::run_healthcheck(&args),
        Cmd::ActiveVersion(args) => edgezero_cli::run_active_version(&args),
        Cmd::Rollback(args) => edgezero_cli::run_rollback(&args),
    };
    if let Err(error) = result {
        eprintln!("[fixture-app] {error}");
        std::process::exit(2);
    }
}
RS

  cat >edgezero.toml <<'TOML'
[app]
name = "fixture-app"

[adapters.fastly.adapter]
manifest = "adapter/fastly.toml"
TOML
  if [[ "$mode" == store-aware ]]; then
    cat >>edgezero.toml <<'TOML'

[stores.config]
ids = ["app_config"]
default = "app_config"

[stores.kv]
ids = ["cache"]
default = "cache"

[stores.secrets]
ids = ["credentials"]
default = "credentials"
TOML
  fi

  mkdir -p adapter
  cat >adapter/fastly.toml <<'TOML'
manifest_version = 3
name = "fixture-app"
language = "rust"
TOML

  cargo generate-lockfile
  git add -A
  git commit -q -m fixture
}

package_release() {
  local cli_archive="$1"
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local app_dir="$workspace/fixture-app"
  local output_dir="$workspace/fixture-release"
  local stage="$output_dir/root"

  [[ -f "$cli_archive" && ! -L "$cli_archive" ]] ||
    fail "application CLI archive is missing or is not a regular file"
  [[ -f "$app_dir/edgezero.toml" && -f "$app_dir/adapter/fastly.toml" ]] ||
    fail "run the source fixture mode before packaging its release"

  mkdir -p "$stage/cli" "$stage/package" "$stage/adapter"
  cp "$cli_archive" "$stage/cli/app-cli.tar"
  cp "$app_dir/edgezero.toml" "$stage/edgezero.toml"
  cp "$app_dir/adapter/fastly.toml" "$stage/adapter/fastly.toml"
  printf 'immutable fixture Fastly package\n' >"$stage/package/app.tar.gz"

  local cli_digest package_digest edgezero_digest adapter_digest revision
  cli_digest=$(sha256_file "$stage/cli/app-cli.tar")
  package_digest=$(sha256_file "$stage/package/app.tar.gz")
  edgezero_digest=$(sha256_file "$stage/edgezero.toml")
  adapter_digest=$(sha256_file "$stage/adapter/fastly.toml")
  revision=$(git -C "$app_dir" rev-parse HEAD)

  jq -n \
    --arg revision "$revision" \
    --arg cli "$cli_digest" \
    --arg package "$package_digest" \
    --arg edgezero "$edgezero_digest" \
    --arg adapter "$adapter_digest" \
    '{
      format: 1,
      source_revision: $revision,
      adapter: "fastly",
      app_cli: {path: "cli/app-cli.tar", sha256: $cli},
      package: {path: "package/app.tar.gz", sha256: $package},
      manifests: {
        edgezero: {path: "edgezero.toml", sha256: $edgezero},
        adapter: {path: "adapter/fastly.toml", sha256: $adapter}
      }
    }' >"$stage/release.json"

  tar -C "$stage" -czf "$output_dir/app-release.tar.gz" \
    release.json cli/app-cli.tar package/app.tar.gz edgezero.toml adapter/fastly.toml
  local release_digest
  release_digest=$(sha256_file "$output_dir/app-release.tar.gz")
  printf '%s\n' "$release_digest" >"$output_dir/app-release.sha256"
  printf '%s\n' "$package_digest" >"$output_dir/package.sha256"
  append_output app-release-sha256 "$release_digest"
  append_output package-digest "$package_digest"
}

main() {
  case "${1:-source}" in
    source) write_source_fixture "${2:-store-aware}" ;;
    release)
      [[ $# -eq 2 ]] || fail "usage: make-smoke-fixture.sh release <app-cli.tar>"
      package_release "$2"
      ;;
    *) fail "usage: make-smoke-fixture.sh source <store-free|store-aware> | release <app-cli.tar>" ;;
  esac
}

main "$@"
