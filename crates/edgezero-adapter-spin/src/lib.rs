//! Adapter helpers for Spin (Fermyon).

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
pub mod config_store;
pub mod context;
mod decompress;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod key_value_store;
// `kv_pagination` is the pure paging logic for `SpinKvStore::list_keys_page`.
// It is host-compilable so its tests run under `cargo test`, while the wasm32
// `SpinKvStore` is the production consumer.
mod kv_pagination;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod proxy;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod request;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod response;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod secret_store;
/// Seed handler for `config push --adapter spin`. Compiled under the
/// same gate as the other wasm-runtime modules; an extra `test` arm
/// keeps the host-compilable core + its unit tests in scope under
/// `cargo test` so the security surface gets covered without
/// requiring `--features spin` or a wasm target.
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
pub(crate) mod seed;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use core::future::Future;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use core::pin::Pin;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use bytes::Bytes;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use edgezero_core::app::{App, Hooks};
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use edgezero_core::env_config::EnvConfig;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use spin_sdk::http::{FullBody, Request as SpinRequest, Response as SpinResponse};

/// Spin SDK response with a fully-buffered body. Extracted as a type alias
/// because the full `Response<FullBody<Bytes>>` form appears in multiple
/// signatures (`AppExt::dispatch`, `request::dispatch*`, `from_core_response`).
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub type SpinFullResponse = SpinResponse<FullBody<Bytes>>;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub trait AppExt {
    /// Dispatch a Spin request through the `EdgeZero` router and return a
    /// fully-buffered Spin response.
    fn dispatch<'app>(
        &'app self,
        req: SpinRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SpinFullResponse>> + 'app>>;
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
impl AppExt for App {
    #[inline]
    fn dispatch<'app>(
        &'app self,
        req: SpinRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SpinFullResponse>> + 'app>> {
        Box::pin(request::dispatch(self, req))
    }
}

/// Initialize the logger for Spin.
///
/// Currently a no-op — Spin manages its own logging internally.
/// When a real logger is needed for one target, split this into
/// `#[cfg(all(feature = "spin", target_arch = "wasm32"))]` /
/// `#[cfg(not(...))]` branches following the Fastly/Cloudflare pattern.
// TODO: wire in real Spin logger when available
///
/// # Errors
/// Returns [`log::SetLoggerError`] if a global logger is already installed.
#[inline]
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

/// Convenience entry point: build the app from `Hooks`, dispatch the
/// incoming Spin request through the `EdgeZero` router, and return the
/// response.
///
/// Portable store config is baked into `A` by the `app!` macro; the KV store
/// label is resolved at runtime from `EDGEZERO__STORES__KV__<ID>__NAME`. No
/// `edgezero.toml` is required.
///
/// Usage in a Spin component:
///
/// ```ignore
/// use spin_sdk::http_service;
/// use my_core::App;
///
/// #[http_service]
/// async fn handle(req: spin_sdk::http::Request) -> anyhow::Result<impl spin_sdk::http::IntoResponse> {
///     edgezero_adapter_spin::run_app::<App>(req).await
/// }
/// ```
///
/// Returns the concrete [`SpinFullResponse`] (was `impl IntoResponse` up
/// through 2026-Q2). Source-compatible with the generated scaffold handler
/// signature because `SpinFullResponse: spin_sdk::http::IntoResponse`.
///
/// # Errors
/// Returns [`anyhow::Error`] when the inner dispatch fails — transport,
/// router, store binding, or response translation errors propagate here.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[inline]
pub async fn run_app<A: Hooks>(req: SpinRequest) -> anyhow::Result<SpinFullResponse> {
    // Best-effort: every Spin `#[http_service]` re-enters this function, so a
    // second `log::set_logger` call returns Err — drop the result instead of
    // `.expect()` to avoid panicking on every subsequent request.
    drop(init_logger());
    let env = EnvConfig::from_env();
    let stores = A::stores();
    let app = A::build_app();
    request::dispatch_with_registries(&app, req, stores.config, stores.kv, stores.secrets, &env)
        .await
}

/// Convenience entry point that ALSO accepts `config push --adapter spin`
/// seed requests on the canonical `/__edgezero/config/seed` route. Every
/// other request falls through to [`run_app`].
///
/// Scaffolded projects use this entrypoint by default so
/// `config push --adapter spin --local` Just Works against a freshly
/// scaffolded app. Projects that don't want the seeding surface can swap
/// the handler body to call [`run_app`] directly.
///
/// The seed handler reads its token from
/// `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN`; if that variable is unset,
/// blank, whitespace-only, or shorter than 16 bytes, every request to
/// `/__edgezero/config/seed` returns 401 (fail-closed). See seed handler
/// docs for the full security model.
///
/// # Errors
/// Same as [`run_app`] — propagates dispatch failures via `anyhow`.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[inline]
pub async fn run_app_with_seeder<A: Hooks>(req: SpinRequest) -> anyhow::Result<SpinFullResponse> {
    if req.uri().path() == seed::SEED_ROUTE {
        let env = EnvConfig::from_env();
        let token_owned = env
            .get(&["adapters", "spin", "seed_token"])
            .map(str::to_owned);
        let stores = A::stores();
        let labels: Vec<String> = stores
            .config
            .as_ref()
            .map(|meta| {
                meta.ids
                    .iter()
                    .map(|id| env.store_name("config", id))
                    .collect()
            })
            .unwrap_or_default();
        return seed::handle_seed_request_spin(
            req,
            &seed::SpinKvSeedWriter,
            token_owned.as_deref(),
            &labels,
        )
        .await;
    }
    run_app::<A>(req).await
}
