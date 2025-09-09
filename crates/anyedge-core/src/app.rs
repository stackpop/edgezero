use crate::http::{Request, Response};
use crate::middleware::Middleware;
use crate::router::Router;

/// Application builder and request dispatcher.
///
/// Compose middleware and register routes, then call [`App::handle`] with
/// provider-specific requests converted to core [`crate::http::Request`].
pub struct App {
    router: Router,
    middleware: Vec<Box<dyn Middleware + Send + Sync>>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Create a new empty application.
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            middleware: Vec::new(),
        }
    }

    /// Get a mutable reference to the internal router.
    /// Prefer using the convenience helpers like [`App::get`] or [`App::route_with`].
    pub fn router_mut(&mut self) -> &mut Router {
        &mut self.router
    }

    /// Add a middleware component to the application.
    ///
    /// Middleware runs in registration order for every request.
    pub fn middleware<M: Middleware + Send + Sync + 'static>(&mut self, m: M) -> &mut Self {
        self.middleware.push(Box::new(m));
        self
    }

    /// Register a GET route with default options.
    pub fn get<H: crate::handler::Handler>(&mut self, path: &str, handler: H) -> &mut Self {
        self.route_with(
            crate::http::Method::GET,
            path,
            handler,
            RouteOptions::default(),
        )
    }
    /// Register a POST route with default options.
    pub fn post<H: crate::handler::Handler>(&mut self, path: &str, handler: H) -> &mut Self {
        self.route_with(
            crate::http::Method::POST,
            path,
            handler,
            RouteOptions::default(),
        )
    }
    /// Register a PUT route with default options.
    pub fn put<H: crate::handler::Handler>(&mut self, path: &str, handler: H) -> &mut Self {
        self.route_with(
            crate::http::Method::PUT,
            path,
            handler,
            RouteOptions::default(),
        )
    }
    /// Register a DELETE route with default options.
    pub fn delete<H: crate::handler::Handler>(&mut self, path: &str, handler: H) -> &mut Self {
        self.route_with(
            crate::http::Method::DELETE,
            path,
            handler,
            RouteOptions::default(),
        )
    }

    // Note: For advanced route configuration (e.g., streaming/buffered policy), use `route_with`.

    /// Dispatch a request through middleware and router to produce a response.
    pub fn handle(&self, req: Request) -> Response {
        fn call_chain(app: &App, idx: usize, req: Request) -> Response {
            if idx >= app.middleware.len() {
                return app.router.route(req);
            }
            let next = |req: Request| call_chain(app, idx + 1, req);
            app.middleware[idx].handle(req, &next)
        }
        call_chain(self, 0, req)
    }

    /// Initialize logging for this process using the provided strategy.
    /// This does not create a logger per-app; logging is process-global in Rust.
    pub fn init_logging(&self, init: crate::logging::LoggerInit) {
        crate::logging::Logging::init_with(init);
    }

    /// Generic route registration with options (preferred for advanced use).
    ///
    /// Use this to configure route policies like streaming/buffered behavior.
    ///
    /// Example: streaming route
    /// ```
    /// # use anyedge_core::{App, Method, Response};
    /// # let mut app = App::new();
    /// use anyedge_core::app::RouteOptions;
    /// app.route_with(Method::GET, "/stream", |_req| {
    ///     let chunks = (0..3).map(|i| format!("{}\n", i).into_bytes());
    ///     Response::ok().with_chunks(chunks)
    /// }, RouteOptions::streaming());
    /// ```
    pub fn route_with<H: crate::handler::Handler>(
        &mut self,
        method: crate::http::Method,
        path: &str,
        handler: H,
        opts: RouteOptions,
    ) -> &mut Self {
        self.router.add(method, path, handler, opts.body_mode);
        self
    }
}

/// Route configuration options.
///
/// - `streaming()`: force streaming responses (buffered bodies are coerced).
/// - `buffered()`: disallow streaming (returns HTTP 500 if a handler streams).
pub struct RouteOptions {
    pub body_mode: crate::router::BodyMode,
}

impl Default for RouteOptions {
    fn default() -> Self {
        Self {
            body_mode: crate::router::BodyMode::Auto,
        }
    }
}

impl RouteOptions {
    /// Force streaming responses for this route.
    pub fn streaming() -> Self {
        Self {
            body_mode: crate::router::BodyMode::Streaming,
        }
    }
    /// Disallow streaming; handlers must return buffered bodies.
    pub fn buffered() -> Self {
        Self {
            body_mode: crate::router::BodyMode::Buffered,
        }
    }
}
