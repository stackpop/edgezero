pub mod config;
// `handlers` is `pub` so downstream integration tests
// can dispatch them directly against a wired `ConfigRegistry` /
// `KvRegistry` / `SecretRegistry` — the same fixture shape the
// runtime sets up. This avoids spinning a real HTTP server in
// tests that only need to verify the push → read-back → handler
// contract end to end. The `app!` macro still uses the handlers
// internally; pub visibility is purely additive.
pub mod handlers;

use std::sync::{Arc, OnceLock};

/// App-owned shared state for the `app!(..., state = ...)` demonstration,
/// handed to handlers via `State<Arc<DemoState>>`.
#[derive(Debug)]
pub struct DemoState {
    /// A greeting the handler echoes, proving the value reached the handler.
    pub greeting: String,
}

/// Returns the shared app state, referenced by `app!(..., state = crate::app_state())`.
///
/// IMPORTANT: `app!(state = <expr>)` emits this call inside the macro-generated
/// `build_router()`, which every adapter's `run_app` invokes via `A::build_app()`
/// — once at startup for long-lived runtimes (Axum), but **once per request** on
/// Fastly Compute (each request is a fresh Wasm instance). So `app_state()` must
/// be **cheap**: build the heavy state once and hand out clones. Here a
/// `OnceLock<Arc<DemoState>>` builds it lazily and every call just bumps the
/// `Arc` refcount — do NOT `Arc::new(..)` a heavy object on each call.
#[must_use]
#[inline]
pub fn app_state() -> Arc<DemoState> {
    static STATE: OnceLock<Arc<DemoState>> = OnceLock::new();
    Arc::clone(STATE.get_or_init(|| {
        Arc::new(DemoState {
            greeting: "hello from app state".to_owned(),
        })
    }))
}

edgezero_core::app!("../../edgezero.toml", state = crate::app_state());
