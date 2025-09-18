use crate::route_set::RouteSet;
use anyedge_core::App;

/// Builder for composing application routes in a controller-first style.
pub struct AppRoutes {
    routes: RouteSet,
}

impl AppRoutes {
    /// Create an empty route builder.
    pub fn new() -> Self {
        Self {
            routes: RouteSet::new(),
        }
    }

    /// Create a route builder with AnyEdge's default routes.
    /// Currently this is identical to [`AppRoutes::new`], but exists to mirror
    /// loco.rs ergonomics and allows us to introduce defaults later without
    /// breaking the API.
    pub fn with_default_routes() -> Self {
        Self::new()
    }

    /// Prefix all routes in this builder.
    pub fn prefix(mut self, prefix: &str) -> Self {
        self.routes = self.routes.prefix(prefix);
        self
    }

    /// Merge another [`RouteSet`] into this builder.
    pub fn add_route(mut self, routes: RouteSet) -> Self {
        self.routes.merge(routes);
        self
    }

    /// Consume the builder and apply routes to an [`App`].
    pub fn apply(self, app: &mut App) {
        self.routes.apply(app);
    }

    /// Consume the builder and return the composed [`RouteSet`].
    pub fn into_route_set(self) -> RouteSet {
        self.routes
    }
}

/// Trait mirroring loco.rs `Hooks` to let AnyEdge apps register routes in a
/// familiar way.
pub trait Hooks: Send + Sync + 'static {
    /// Name of the application (defaults to crate name).
    fn app_name() -> &'static str {
        env!("CARGO_CRATE_NAME")
    }

    /// Compose the routes for this application.
    fn routes() -> AppRoutes;

    /// Configure an [`App`] before routes are applied.
    fn configure(_app: &mut App) {}

    /// Build an [`App`] for this Hooks implementation.
    fn build_app() -> App {
        let mut app = App::new();
        Self::configure(&mut app);
        Self::routes().apply(&mut app);
        app
    }
}
