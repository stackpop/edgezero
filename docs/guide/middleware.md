# Middleware

EdgeZero supports composable middleware for cross-cutting concerns like logging, authentication, and CORS.

## Defining Middleware

Middleware implements the `Middleware` trait:

```rust
use edgezero_core::middleware::Middleware;
use edgezero_core::http::{Request, Response};
use edgezero_core::body::Body;

pub struct RequestLogger;

impl Middleware for RequestLogger {
    async fn handle(
        &self,
        req: Request<Body>,
        next: impl FnOnce(Request<Body>) -> futures::future::BoxFuture<'static, Response<Body>> + Send,
    ) -> Response<Body> {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        
        log::info!("--> {} {}", method, path);
        
        let response = next(req).await;
        
        log::info!("<-- {} {} {}", method, path, response.status());
        
        response
    }
}
```

## Registering Middleware

### Via Manifest

Define middleware in `edgezero.toml`:

```toml
[app]
name = "my-app"
entry = "crates/my-app-core"
middleware = [
  "edgezero_core::middleware::RequestLogger",
  "my_app_core::middleware::Auth"
]
```

Middleware are applied in order before routes are matched.

### Programmatically

Register middleware when building the router:

```rust
use edgezero_core::router::RouterService;

let router = RouterService::builder()
    .middleware(RequestLogger)
    .middleware(CorsMiddleware::default())
    .route(Method::GET, "/hello", hello)
    .build();
```

## Middleware Order

Middleware execute in registration order for requests, and reverse order for responses:

```
Request Flow:
  Client → Logger → Auth → CORS → Handler

Response Flow:
  Handler → CORS → Auth → Logger → Client
```

## Common Patterns

### Authentication

```rust
pub struct AuthMiddleware {
    secret: String,
}

impl Middleware for AuthMiddleware {
    async fn handle(
        &self,
        req: Request<Body>,
        next: impl FnOnce(Request<Body>) -> futures::future::BoxFuture<'static, Response<Body>> + Send,
    ) -> Response<Body> {
        // Check authorization header
        let auth_header = req.headers().get("authorization");
        
        match auth_header {
            Some(value) if self.verify_token(value) => {
                // Token valid, continue to handler
                next(req).await
            }
            _ => {
                // Return 401 Unauthorized
                Response::builder()
                    .status(401)
                    .body(Body::from("Unauthorized"))
                    .unwrap()
            }
        }
    }
}
```

### CORS

```rust
pub struct CorsMiddleware {
    allowed_origins: Vec<String>,
}

impl Middleware for CorsMiddleware {
    async fn handle(
        &self,
        req: Request<Body>,
        next: impl FnOnce(Request<Body>) -> futures::future::BoxFuture<'static, Response<Body>> + Send,
    ) -> Response<Body> {
        let origin = req.headers()
            .get("origin")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        
        let mut response = next(req).await;
        
        if let Some(origin) = origin {
            if self.allowed_origins.contains(&origin) {
                response.headers_mut().insert(
                    "access-control-allow-origin",
                    origin.parse().unwrap(),
                );
            }
        }
        
        response
    }
}
```

### Request Timing

```rust
pub struct TimingMiddleware;

impl Middleware for TimingMiddleware {
    async fn handle(
        &self,
        req: Request<Body>,
        next: impl FnOnce(Request<Body>) -> futures::future::BoxFuture<'static, Response<Body>> + Send,
    ) -> Response<Body> {
        let start = std::time::Instant::now();
        
        let mut response = next(req).await;
        
        let duration = start.elapsed();
        response.headers_mut().insert(
            "x-response-time",
            format!("{}ms", duration.as_millis()).parse().unwrap(),
        );
        
        response
    }
}
```

## Early Returns

Middleware can short-circuit the chain by not calling `next`:

```rust
impl Middleware for RateLimiter {
    async fn handle(
        &self,
        req: Request<Body>,
        next: impl FnOnce(Request<Body>) -> futures::future::BoxFuture<'static, Response<Body>> + Send,
    ) -> Response<Body> {
        if self.is_rate_limited(&req) {
            // Don't call next - return immediately
            return Response::builder()
                .status(429)
                .body(Body::from("Too Many Requests"))
                .unwrap();
        }
        
        next(req).await
    }
}
```

## Built-in Middleware

EdgeZero provides these middleware out of the box:

| Middleware | Purpose |
|------------|---------|
| `RequestLogger` | Logs request method, path, and response status |

## Next Steps

- Learn about [Streaming](/guide/streaming) for progressive responses
- Explore [Proxying](/guide/proxying) for upstream forwarding
