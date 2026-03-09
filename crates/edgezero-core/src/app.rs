use crate::router::RouterService;

const DEFAULT_APP_NAME: &str = "EdgeZero App";

/// Canonical adapter name for the Axum adapter.
pub const AXUM_ADAPTER: &str = "axum";
/// Canonical adapter name for the Cloudflare adapter.
pub const CLOUDFLARE_ADAPTER: &str = "cloudflare";
/// Canonical adapter name for the Fastly adapter.
pub const FASTLY_ADAPTER: &str = "fastly";

/// Adapter-specific config-store override metadata generated from `[stores.config.adapters.*]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigStoreAdapterMetadata {
    adapter: &'static str,
    name: &'static str,
}

impl ConfigStoreAdapterMetadata {
    pub const fn new(adapter: &'static str, name: &'static str) -> Self {
        Self { adapter, name }
    }

    pub fn adapter(&self) -> &'static str {
        self.adapter
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// Provider-neutral config-store metadata generated from `[stores.config]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigStoreMetadata {
    default_name: &'static str,
    adapters: &'static [ConfigStoreAdapterMetadata],
}

impl ConfigStoreMetadata {
    pub const fn new(
        default_name: &'static str,
        adapters: &'static [ConfigStoreAdapterMetadata],
    ) -> Self {
        Self {
            default_name,
            adapters,
        }
    }

    pub fn default_name(&self) -> &'static str {
        self.default_name
    }

    pub fn adapters(&self) -> &'static [ConfigStoreAdapterMetadata] {
        self.adapters
    }

    pub fn name_for_adapter(&self, adapter: &str) -> &'static str {
        self.adapters
            .iter()
            .find(|entry| entry.adapter.eq_ignore_ascii_case(adapter))
            .map(|entry| entry.name)
            .unwrap_or(self.default_name)
    }
}

/// Lightweight container around a `RouterService` that can be extended via hook implementations.
pub struct App {
    router: RouterService,
    name: String,
}

impl App {
    /// Create a new application wrapper from the supplied router service.
    pub fn new(router: RouterService) -> Self {
        Self::with_name(router, DEFAULT_APP_NAME)
    }

    /// Access the underlying router service.
    pub fn router(&self) -> &RouterService {
        &self.router
    }

    /// Name assigned to the application.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Update the application name.
    pub fn set_name<S>(&mut self, name: S)
    where
        S: Into<String>,
    {
        self.name = name.into();
    }

    /// Consume the app and return the contained router service.
    pub fn into_router(self) -> RouterService {
        self.router
    }

    /// Construct a new application with the provided router and name.
    pub fn with_name<S>(router: RouterService, name: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            router,
            name: name.into(),
        }
    }

    /// Default name used when none is provided.
    pub fn default_name() -> &'static str {
        DEFAULT_APP_NAME
    }
}

/// Trait implemented by application hook adapters.
pub trait Hooks {
    /// Allow implementations to mutate the freshly constructed application before use.
    /// The default implementation performs no changes.
    fn configure(_app: &mut App) {}

    /// Build the router service for the application.
    fn routes() -> RouterService;

    /// Display name for the application. Defaults to `"EdgeZero App"`.
    fn name() -> &'static str {
        App::default_name()
    }

    /// Structured config-store metadata for the application, if declared.
    ///
    /// Macro-generated apps derive this from `[stores.config]` in `edgezero.toml`.
    fn config_store() -> Option<&'static ConfigStoreMetadata> {
        None
    }

    /// Construct an `App` by wiring the routes and invoking the configuration hook.
    fn build_app() -> App
    where
        Self: Sized,
    {
        let mut app = App::with_name(Self::routes(), Self::name());
        Self::configure(&mut app);
        app
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::context::RequestContext;
    use crate::error::EdgeError;
    use crate::http::{request_builder, Method, StatusCode};
    use futures::executor::block_on;
    use tower_service::Service;

    fn empty_router() -> RouterService {
        RouterService::builder().build()
    }

    #[test]
    fn default_app_uses_constant_name() {
        let app = App::new(empty_router());
        assert_eq!(app.name(), App::default_name());
    }

    struct TestHooks;

    impl Hooks for TestHooks {
        fn routes() -> RouterService {
            async fn handler(_ctx: RequestContext) -> Result<String, EdgeError> {
                Ok("ok".to_string())
            }

            RouterService::builder().get("/test", handler).build()
        }

        fn configure(app: &mut App) {
            app.set_name("configured");
        }

        fn name() -> &'static str {
            "hooks-name"
        }

        fn config_store() -> Option<&'static ConfigStoreMetadata> {
            static CONFIG_STORE: ConfigStoreMetadata = ConfigStoreMetadata::new(
                "default-config",
                &[ConfigStoreAdapterMetadata::new(
                    CLOUDFLARE_ADAPTER,
                    "cf-config",
                )],
            );
            Some(&CONFIG_STORE)
        }
    }

    #[test]
    fn build_app_invokes_hooks_for_routes_and_configuration() {
        let app = TestHooks::build_app();
        assert_eq!(app.name(), "configured");
        let config = TestHooks::config_store().expect("config store metadata");
        assert_eq!(config.name_for_adapter(CLOUDFLARE_ADAPTER), "cf-config");
        assert_eq!(config.name_for_adapter("CLOUDFLARE"), "cf-config");
        assert_eq!(config.name_for_adapter(FASTLY_ADAPTER), "default-config");
        assert_eq!(config.default_name(), "default-config");
        assert_eq!(config.adapters().len(), 1);

        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");

        let response = block_on(app.router().clone().call(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes(), b"ok");
    }

    struct DefaultHooks;

    impl Hooks for DefaultHooks {
        fn routes() -> RouterService {
            RouterService::builder().build()
        }
    }

    #[test]
    fn default_hooks_use_default_name_and_into_router() {
        let app = DefaultHooks::build_app();
        assert_eq!(app.name(), App::default_name());
        assert_eq!(DefaultHooks::config_store(), None);
        let router = app.into_router();
        assert!(router.routes().is_empty());
    }
}
