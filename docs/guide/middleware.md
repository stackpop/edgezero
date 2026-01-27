# Middleware

EdgeZero supports composable middleware for cross-cutting concerns like logging, authentication, and CORS.

## Defining Middleware

Middleware implements the `Middleware` trait:

```rust
use async_trait::async_trait;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Response;
use edgezero_core::middleware::{Middleware, Next};

pub struct RequestLogger;

#[async_trait(?Send)]
impl Middleware for RequestLogger {
    async fn handle(
        &self,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<Response, EdgeError> {
        let method = ctx.request().method().clone();
        let path = ctx.request().uri().path().to_string();
        let start = std::time::Instant::now();

        let response = next.run(ctx).await?;

        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(
            "request method={} path={} status={} elapsed_ms={:.2}",
            method,
            path,
            response.status().as_u16(),
            elapsed_ms
        );

        Ok(response)
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
    .get("/hello", hello)
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
use edgezero_core::body::Body;

pub struct AuthMiddleware {
    secret: String,
}

#[async_trait(?Send)]
impl Middleware for AuthMiddleware {
    async fn handle(
        &self,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<Response, EdgeError> {
        // Check authorization header
        let auth_header = ctx.request().headers().get("authorization");

        match auth_header {
            Some(value) if self.verify_token(value) => {
                // Token valid, continue to handler
                next.run(ctx).await
            }
            _ => {
                // Return 401 Unauthorized
                let response = Response::builder()
                    .status(401)
                    .body(Body::from("Unauthorized"))
                    .map_err(EdgeError::internal)?;
                Ok(response)
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

#[async_trait(?Send)]
impl Middleware for CorsMiddleware {
    async fn handle(
        &self,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<Response, EdgeError> {
        let origin = ctx
            .request()
            .headers()
            .get("origin")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let mut response = next.run(ctx).await?;

        if let Some(origin) = origin {
            if self.allowed_origins.contains(&origin) {
                response.headers_mut().insert(
                    "access-control-allow-origin",
                    origin.parse().unwrap(),
                );
            }
        }

        Ok(response)
    }
}
```

### Request Timing

```rust
pub struct TimingMiddleware;

#[async_trait(?Send)]
impl Middleware for TimingMiddleware {
    async fn handle(
        &self,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<Response, EdgeError> {
        let start = std::time::Instant::now();

        let mut response = next.run(ctx).await?;

        let duration = start.elapsed();
        response.headers_mut().insert(
            "x-response-time",
            format!("{}ms", duration.as_millis()).parse().unwrap(),
        );

        Ok(response)
    }
}
```

## Early Returns

Middleware can short-circuit the chain by not calling `next`:

```rust
use edgezero_core::body::Body;

impl Middleware for RateLimiter {
    async fn handle(
        &self,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<Response, EdgeError> {
        if self.is_rate_limited(&ctx) {
            // Don't call next - return immediately
            let response = Response::builder()
                .status(429)
                .body(Body::from("Too Many Requests"))
                .map_err(EdgeError::internal)?;
            return Ok(response);
        }

        next.run(ctx).await
    }
}
```

## Built-in Middleware

EdgeZero provides these middleware out of the box:

| Middleware      | Purpose                                        |
| --------------- | ---------------------------------------------- |
| `RequestLogger` | Logs request method, path, and response status |

## Next Steps

- Learn about [Streaming](/guide/streaming) for progressive responses
- Explore [Proxying](/guide/proxying) for upstream forwarding
