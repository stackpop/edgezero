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
