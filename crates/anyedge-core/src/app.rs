use crate::RouterService;

const DEFAULT_APP_NAME: &str = "AnyEdge App";

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

/// Trait implemented by application hook providers.
pub trait Hooks {
    /// Allow implementations to mutate the freshly constructed application before use.
    /// The default implementation performs no changes.
    fn configure(_app: &mut App) {}

    /// Build the router service for the application.
    fn routes() -> RouterService;

    /// Display name for the application. Defaults to `"AnyEdge App"`.
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
