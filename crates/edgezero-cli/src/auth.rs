//! `auth` command.
//!
//! Pure thin delegate to the adapter registry — the same dispatch
//! path `build` / `deploy` / `serve` use. The CLI does NOT know how
//! `cloudflare` / `fastly` / `spin` sign in; each adapter crate owns
//! its own implementation (shell out to `wrangler login`, hit an HTTP
//! API, whatever) inside its `Adapter::execute` impl. Per-project
//! overrides live in `[adapters.<name>.commands].auth-{login,logout,
//! status}` in `edgezero.toml`; `axum` is a no-op (no remote auth).

use crate::adapter::{self, Action};
use crate::args::{AuthArgs, AuthSub};
use crate::{ensure_adapter_defined, load_manifest_optional};

/// Sign in / out / status against the adapter's native auth surface.
///
/// # Errors
///
/// Returns an error if the manifest cannot be loaded, the adapter is
/// not registered (or its name is unknown), or the adapter's auth
/// dispatch fails (missing CLI on PATH, non-zero exit, etc.).
#[inline]
pub fn run_auth(args: &AuthArgs) -> Result<(), String> {
    let (adapter_name, action) = match &args.sub {
        AuthSub::Login { adapter } => (adapter.as_str(), Action::AuthLogin),
        AuthSub::Logout { adapter } => (adapter.as_str(), Action::AuthLogout),
        AuthSub::Status { adapter } => (adapter.as_str(), Action::AuthStatus),
    };

    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(adapter_name, manifest.as_ref())?;
    adapter::execute(adapter_name, action, manifest.as_ref(), &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{AuthArgs, AuthSub};
    use crate::test_support::{BASIC_MANIFEST, EnvOverride, manifest_guard};
    use std::fs;
    use tempfile::TempDir;

    /// Auth dispatches through the same `adapter::execute` path as
    /// `build` / `deploy` / `serve`, so the orchestration test
    /// follows the same shape — configure the manifest's
    /// `auth-{login,logout,status}` override to a harmless `echo`
    /// command and assert each subcommand runs cleanly. The real
    /// per-adapter implementations (`wrangler login`, etc.) live in
    /// the adapter crates and are not exercised in CI.
    #[cfg(not(windows))]
    #[test]
    fn run_auth_dispatches_each_subcommand_via_manifest_override() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        for sub in [
            AuthSub::Login {
                adapter: "fastly".to_owned(),
            },
            AuthSub::Logout {
                adapter: "fastly".to_owned(),
            },
            AuthSub::Status {
                adapter: "fastly".to_owned(),
            },
        ] {
            run_auth(&AuthArgs { sub }).expect("auth subcommand runs");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn run_auth_rejects_unknown_adapter() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        let err = run_auth(&AuthArgs {
            sub: AuthSub::Login {
                adapter: "wat".to_owned(),
            },
        })
        .expect_err("unknown adapter must error");
        assert!(
            err.contains("wat"),
            "error should name the unknown adapter: {err}"
        );
    }
}
