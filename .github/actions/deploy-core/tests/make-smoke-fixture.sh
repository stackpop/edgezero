#!/usr/bin/env bash
set -euo pipefail

# Builds the fixture the composite smoke test deploys.
#
# This is a REAL app-owned CLI: a standalone Cargo workspace (kept out of the
# surrounding edgezero workspace) whose own crate depends on `edgezero-cli` and
# exposes deploy / healthcheck / rollback. That exercises the actual contract —
# "the application provides the CLI package" — instead of building the monorepo's
# own CLI.
#
# The Fastly deploy command is overridden by a marker script that emits
# `version=<N>` (version threading), records the credentials it actually saw
# (provider-env boundary), and records its argv — all without contacting Fastly.
#
# Inputs (environment): GITHUB_WORKSPACE (required).

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local app_dir="$workspace/fixture-app"

  mkdir -p "$app_dir/crates/fixture-app-cli/src"
  cd "$app_dir"

  git init -q
  git config user.email test@example.com
  git config user.name Test

  # Standalone workspace: not a member of the surrounding edgezero workspace.
  cat >Cargo.toml <<'TOML'
[workspace]
members = ["crates/fixture-app-cli"]
resolver = "2"
TOML

  # The app's OWN CLI crate, built on edgezero-cli (path dep into the checkout).
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
//! Fixture app CLI: the smoke test's stand-in for an application-owned CLI.
//!
//! It wires the TYPED `config push` (not the bundled stub), because that is the
//! contract config-push-fastly depends on: only an app-owned CLI has the
//! app-config struct, so only it can push typed config.
use clap::{Parser, Subcommand};
use edgezero_cli::args::{
    ActiveVersionArgs, BuildArgs, ConfigPushArgs, DeployArgs, HealthcheckArgs, RollbackArgs,
};
use serde::{Deserialize, Serialize};
use validator::Validate;

/// The fixture's typed app config, loaded from `fixture-app.toml`.
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
    if let Err(err) = result {
        eprintln!("[fixture-app] {err}");
        std::process::exit(2);
    }
}
RS

  # Marker "deploy" the CLI runs instead of `fastly compute deploy`. It records
  # the credentials it saw and its argv, and emits a version line.
  cat >fake-deploy.sh <<'SH'
#!/usr/bin/env bash
{
  printf 'token=%s\n' "${FASTLY_API_TOKEN:-MISSING}"
  printf 'service-id=%s\n' "${FASTLY_SERVICE_ID:-MISSING}"
  # Boundary: inherited provider aliases must have been cleared...
  printf 'endpoint=%s\n' "${FASTLY_ENDPOINT:-CLEARED}"
  printf 'home=%s\n' "${FASTLY_HOME:-CLEARED}"
  # ...and the action's own secret-bearing helpers must NOT have survived into
  # this process: they carry the raw token under names we never promised.
  printf 'action-token-carrier=%s\n' "${EDGEZERO__FASTLY__API_TOKEN:-CLEARED}"
  printf 'provider-env-json=%s\n' "${EDGEZERO__PROVIDER__ENV:-CLEARED}"
} >"${GITHUB_WORKSPACE}/fixture-app/env-seen.txt"
printf '%s\n' "$@" >"${GITHUB_WORKSPACE}/fixture-app/deploy-argv.txt"
# Reflect the activation in the fake Fastly API's state: this "deploy" makes
# version 7 the active one, so the production-rollback guard (which requires the
# rolled-back-from --version to still be active) sees 7 rather than the 40 that
# capture saw before this deploy.
[ -n "${FAKE_ACTIVE_VERSION_FILE:-}" ] && printf '7\n' >"${FAKE_ACTIVE_VERSION_FILE}"
# Recovery smoke: model a MUTATION whose version line is LOST. The service is now
# at 7 (activated above), but the deploy emits no parseable `version=`, so
# deploy-fastly fails while `mutation-attempted` stays true. FAKE_LOSE_VERSION is
# outside the EDGEZERO__* namespace, so it survives the pre-exec scrub.
if [ -n "${FAKE_LOSE_VERSION:-}" ]; then
  echo "deploy mutated the service but its version line was lost" >&2
  exit 0
fi
echo "version=7"
SH
  chmod +x fake-deploy.sh

  # A credential-free "build" the cache-seed step runs under build-mode: always. It
  # does no real compile — it just populates target/ (the cache path) so there is
  # something to save, and it is IDEMPOTENT: if the marker already exists (restored
  # from cache) it leaves it untouched, which is how the cache smoke proves a restore
  # HIT rather than a rebuild. It must run WITHOUT a provider token.
  cat >fake-build.sh <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[ -z "${FASTLY_API_TOKEN:-}" ] || {
  echo "cache-seed build must run without a provider token" >&2
  exit 91
}
mkdir -p target
# Idempotent: only stamp a fresh marker when one was NOT restored from the cache.
if [ ! -f target/fixture-build-marker ]; then
  printf 'built-%s\n' "$RANDOM$RANDOM" >target/fixture-build-marker
fi
echo "Built package (fixture cache seed)"
SH
  chmod +x fake-build.sh

  cat >edgezero.toml <<'ETOML'
[app]
name = "fixture-app"

[adapters.fastly.commands]
build = "bash fake-build.sh"
deploy = "bash fake-deploy.sh"

# config push resolves this logical id, then the Fastly adapter matches it by
# name against `fastly config-store list --json`.
[stores.config]
ids = ["app_config"]
default = "app_config"
ETOML

  # The typed app config `config push` reads (named from `[app].name`).
  cat >fixture-app.toml <<'ATOML'
greeting = "hello from the fixture"
ATOML

  # The staged-deploy path bypasses manifest commands and drives the Fastly CLI,
  # so it needs a Fastly manifest to resolve its working directory.
  cat >fastly.toml <<'FTOML'
manifest_version = 3
name = "fixture-app"
language = "rust"
FTOML

  # Ignore build output AND the fake deploy's side-effect files (env-seen.txt,
  # deploy-argv.txt) so they never dirty the source and trip the committed-source
  # guard. This matters for the cache smoke, which deploys TWICE: the first deploy
  # writes these, and the second deploy's guard would otherwise see a dirty tree.
  printf 'target/\nenv-seen.txt\ndeploy-argv.txt\n' >.gitignore

  cargo generate-lockfile

  git add -A
  git commit -q -m fixture
}

main "$@"
