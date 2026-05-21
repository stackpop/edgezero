#![cfg(feature = "demo-example")]

//! The `edgezero demo` subcommand.
//!
//! `demo` runs the bundled `app-demo` example locally — the **same way**
//! `app-demo`'s own axum adapter runs it: via
//! [`edgezero_adapter_axum::dev_server::run_app`], which loads
//! `app-demo`'s `edgezero.toml` and wires the full setup (routing, KV /
//! config / secret stores, logging, host/port).
//!
//! This is a contributor-only convenience: it depends on the in-repo
//! `examples/app-demo` crate, so it is compiled only under the
//! `demo-example` feature and is not part of any shipped CLI.

/// Run the bundled `app-demo` example on the local axum server.
///
/// Delegates to `run_app`, so `edgezero demo` behaves identically to
/// `cargo run -p app-demo-adapter-axum`.
///
/// # Errors
///
/// Returns an error if the demo server fails to start.
pub fn run_demo() -> Result<(), String> {
    use app_demo_core::App;
    use edgezero_adapter_axum::dev_server::run_app;

    run_app::<App>(include_str!("../../../examples/app-demo/edgezero.toml"))
        .map_err(|err| format!("demo server error: {err}"))
}
