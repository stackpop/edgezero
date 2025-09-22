use crate::RouterService;

/// Lightweight container around a `RouterService` that can be extended via hook implementations.
pub struct App {
    router: RouterService,
}

impl App {
    /// Create a new application wrapper from the supplied router service.
    pub fn new(router: RouterService) -> Self {
        Self { router }
    }

    /// Access the underlying router service.
    pub fn router(&self) -> &RouterService {
        &self.router
    }

    /// Consume the app and return the contained router service.
    pub fn into_router(self) -> RouterService {
        self.router
    }
}

/// Trait implemented by application hook providers.
pub trait Hooks {
    /// Allow implementations to mutate the freshly constructed application before use.
    /// The default implementation performs no changes.
    fn configure(_app: &mut App) {}

    /// Build the router service for the application.
    fn routes() -> RouterService;

    /// Construct an `App` by wiring the routes and invoking the configuration hook.
    fn build_app() -> App
    where
        Self: Sized,
    {
        let mut app = App::new(Self::routes());
        Self::configure(&mut app);
        app
    }
}
