#![cfg(feature = "edgezero-adapter-axum")]

//! The `edgezero demo` subcommand.
//!
//! `demo` runs the bundled `app-demo` example locally — the **same way**
//! `app-demo`'s own axum adapter runs it: via
//! [`edgezero_adapter_axum::dev_server::run_app`], which loads
//! `app-demo`'s `edgezero.toml` and wires the full setup (routing, KV /
//! config / secret stores, logging, host/port). The example is only
//! compiled in under the `dev-example` feature.

/// Run the bundled `app-demo` example on the local axum server.
///
/// Delegates to `run_app`, so `edgezero demo` behaves identically to
/// `cargo run -p app-demo-adapter-axum`.
///
/// # Errors
///
/// Returns an error if the demo server fails to start.
#[cfg(feature = "dev-example")]
pub fn run_demo() -> Result<(), String> {
    use app_demo_core::App;
    use edgezero_adapter_axum::dev_server::run_app;

    run_app::<App>(include_str!("../../../examples/app-demo/edgezero.toml"))
        .map_err(|err| format!("demo server error: {err}"))
}

/// Stand-in for builds without the `dev-example` feature.
///
/// # Errors
///
/// Always errors: the `app-demo` example is not bundled in this build.
#[cfg(not(feature = "dev-example"))]
pub fn run_demo() -> Result<(), String> {
    Err(
        "edgezero demo requires the `dev-example` feature (the app-demo example is not bundled in this build); rebuild with `--features dev-example`."
            .to_owned(),
    )
}
