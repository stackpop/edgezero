pub mod config;
// `handlers` is `pub` so downstream integration tests
// can dispatch them directly against a wired `ConfigRegistry` /
// `KvRegistry` / `SecretRegistry` — the same fixture shape the
// runtime sets up. This avoids spinning a real HTTP server in
// tests that only need to verify the push → read-back → handler
// contract end to end. The `app!` macro still uses the handlers
// internally; pub visibility is purely additive.
pub mod handlers;

use std::sync::Arc;

/// App-owned shared state for the `app!(..., state = ...)` demonstration,
/// handed to handlers via `State<Arc<DemoState>>`.
#[derive(Debug)]
pub struct DemoState {
    /// A greeting the handler echoes, proving the value reached the handler.
    pub greeting: String,
}

/// Constructs the shared app state. Referenced by `app!(..., state = crate::app_state())`.
#[must_use]
#[inline]
pub fn app_state() -> Arc<DemoState> {
    Arc::new(DemoState {
        greeting: "hello from app state".to_owned(),
    })
}

edgezero_core::app!("../../edgezero.toml", state = crate::app_state());
