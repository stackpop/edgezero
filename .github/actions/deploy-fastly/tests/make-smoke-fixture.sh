#!/usr/bin/env bash
set -euo pipefail

# Builds the application-owned CLI source fixture, then packages that CLI with
# immutable Fastly package and manifest bytes into one verified application
# release. Runtime publisher/environment choices are deliberately absent here.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../fastly-common/scripts/common.sh
source "$SCRIPT_DIR/../../fastly-common/scripts/common.sh"

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
  "edgezero-adapter-spin",
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

[adapters.spin.adapter]
manifest = "adapter/spin.toml"
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
  cat >adapter/spin.toml <<'TOML'
spin_manifest_version = 2

[application]
name = "fixture-app"
version = "0.1.0"

[component.fixture]
source = "fixture.wasm"
TOML

  cargo generate-lockfile
  git add -A
  git commit -q -m fixture
}

main() {
  case "${1:-source}" in
    source) write_source_fixture "${2:-store-aware}" ;;
    *) fail "usage: make-smoke-fixture.sh source <store-free|store-aware>" ;;
  esac
}

main "$@"
