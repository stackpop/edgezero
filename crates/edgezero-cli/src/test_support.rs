//! Test-only fixtures shared across `auth`, `provision`, `build`,
//! `deploy`, `serve`, and `config` test modules.
//!
//! Each of those modules calls into the global `EDGEZERO_MANIFEST`
//! env var and the adapter registry, both of which are process-wide
//! state. The `manifest_guard()` mutex serialises tests that touch
//! either; the `EnvOverride` RAII guard restores the prior env value
//! when dropped, so a panic in one test cannot leak state into the
//! next.
//!
//! Kept under `pub(crate)` so the in-module test files (per the
//! "colocate tests with implementation" convention in CLAUDE.md)
//! can share the harness without each duplicating the BASIC /
//! PROVISION manifest fixtures.

use std::sync::{Mutex, OnceLock};

/// RAII guard that overrides a process-global env var for the duration of a
/// test and restores the prior value on drop. Acquire [`manifest_guard`] BEFORE
/// constructing one, and hold it for the guard's lifetime — that is the safety
/// contract documented on `edgezero_core::test_env`.
pub(crate) use edgezero_core::test_env::EnvOverride;

/// `provision` dispatch fixture: declares axum + fastly +
/// cloudflare + spin (every adapter the build registers), with
/// store ids per kind so axum has something to print and the
/// other adapters' stubs are exercised against a non-empty input.
pub(crate) const PROVISION_MANIFEST: &str = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
manifest = "crates/demo-axum/axum.toml"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[adapters.cloudflare.adapter]
crate = "crates/demo-cf"
manifest = "crates/demo-cf/wrangler.toml"

[adapters.cloudflare.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[adapters.fastly.adapter]
crate = "crates/demo-fastly"
manifest = "crates/demo-fastly/fastly.toml"

[adapters.fastly.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "crates/demo-spin/spin.toml"
[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.kv]
ids = ["sessions", "cache"]
default = "sessions"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;

/// Minimal manifest covering the auth + build/deploy/serve dispatch
/// surface. Only fastly is declared because its command overrides
/// (`auth-login` etc.) are what the auth orchestration tests
/// substitute with `echo` to keep CI hermetic.
pub(crate) const BASIC_MANIFEST: &str = r#"
[app]
name = "demo-app"
entry = "crates/demo-core"

[adapters.fastly.adapter]
crate = "crates/demo-fastly"
manifest = "crates/demo-fastly/fastly.toml"

[adapters.fastly.build]
target = "wasm32-unknown-unknown"
profile = "release"

[adapters.fastly.commands]
build = "echo build"
deploy = "echo deploy"
serve = "echo serve"
auth-login = "echo logged in"
auth-logout = "echo logged out"
auth-status = "echo whoami"
"#;

/// Process-wide mutex serialising tests that mutate `EDGEZERO_MANIFEST`
/// or otherwise observe global adapter-registry state. Acquire it
/// BEFORE constructing the `EnvOverride` so two parallel tests
/// don't race the env-var write.
pub(crate) fn manifest_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}
