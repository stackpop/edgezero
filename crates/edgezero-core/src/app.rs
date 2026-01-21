use crate::router::RouterService;

const DEFAULT_APP_NAME: &str = "EdgeZero App";

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
    }

    #[test]
    fn build_app_invokes_hooks_for_routes_and_configuration() {
        let app = TestHooks::build_app();
        assert_eq!(app.name(), "configured");

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
        let router = app.into_router();
        assert!(router.routes().is_empty());
    }
}
