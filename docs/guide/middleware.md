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
        let start = web_time::Instant::now();

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

Routes are matched first. Middleware then run in registration order for a matched route;
unmatched 404/405 requests bypass the middleware chain.

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

`RequestTimingMiddleware<N, D>` attaches a fresh `RequestTimings<N, D>` only when
that exact type is absent. All clones share one portable clock and one mutex,
including the application-owned payload. Register it before timing consumers.
The default excludes no paths; static exclusions compare exact URI paths for every
method (including `/health?check=1`), without removing preinstalled handles.

Applications own their phase enum and map it to bounded slots through a facade:

```rust
use std::time::Duration;
use edgezero_core::context::RequestContext;
use edgezero_core::middleware::RequestTimingMiddleware;
use edgezero_core::request_timing::{RequestTimings, TimingError};
use edgezero_core::router::RouterService;

#[derive(Default)]
struct AppData {
    attempts: u32,
    started: Option<Duration>,
}

type Handle = RequestTimings<1, AppData>;

enum Phase {
    Fetch,
}

impl Phase {
    fn slot(self) -> usize {
        match self {
            Self::Fetch => 0,
        }
    }
}

struct AppTimings(Handle);

impl AppTimings {
    fn from_context(ctx: &RequestContext) -> Option<Self> {
        ctx.request().extensions().get::<Handle>().cloned().map(Self)
    }

    fn record(&self, phase: Phase, duration: Duration) -> Result<(), TimingError> {
        self.0.record_with(phase.slot(), duration, |data, elapsed| {
            data.attempts = data.attempts.saturating_add(1);
            data.started.get_or_insert(elapsed);
        })
    }
}

let router = RouterService::builder()
    .middleware(RequestTimingMiddleware::<1, AppData>::default()
        .with_excluded_paths(&["/health"]))
    .build();
```

Register application routes on that builder. The facade reads `Handle` from
extensions; it is **not** a second installed extension or clock. Do not use
`RouterBuilder::with_state` for collectors: router state is shared across requests
and overwrites same-type request extensions. The external `request_timing_consumer`
test compiles this pattern through a real handler, including a non-`Clone`,
non-`Sync` payload. Middleware payloads need only `Default + Send + 'static`;
manual `with_data(data)` construction needs no `Default`.

- `record(slot, duration)` saturating-adds; absent and recorded zero stay distinct.
  `span(slot)?` records work on drop, including cancelled work, but not completion.
  Destructor execution is not guaranteed in panic-abort environments.
- `record_with` updates phase and payload together; `update_data` updates payload
  alone using elapsed time from the same origin. `snapshot(|view| ...)` projects
  generic fields and borrowed payload under that same lock, without cloning it.
  Borrowed payload cannot escape the projection.
- Invalid slots return `TimingError::InvalidSlot` before mutation or callbacks.
  Contention returns `TimingError::Unavailable`, dropping the whole operation
  without invoking its callback. There is no blocking or retry. Span drop discards
  an unavailable sample. Missing facts must not be interpreted as zero.
- Callbacks must be short, synchronous and non-reentrant: never call a lock-taking
  collector operation inside one. Panics propagate and may leave partial state;
  poisoned locks recover best-effort, **without rollback or payload validation**.
  Application callbacks must keep data valid even on unwind.
- Call `mark_headers_ready()` and `mark_request_elapsed()` at explicit application
  boundaries (first successful write wins); `set_resp_bytes(bytes)` is last-write.
  T0 is collector creation at the chosen boundary, not socket ingress or browser
  navigation. Wrapping or reattaching a handle never resets that origin.

Attachment does not finalize a request, consume a streamed body, render
`Server-Timing`, or change cache/privacy policy. Header rendering and exposure
remain application-owned. Unmatched routes bypass middleware, and
`RouterService::oneshot` renders handler errors outside the chain. Use adapter or
outer-service hooks for terminal/error and streaming measurements as needed.

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

| Middleware                      | Purpose                                                                                                                                                              |
| ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `RequestTimingMiddleware<N, D>` | Attaches a per-request timing collector if absent; configurable exact-path exclusions                                                                                |
| `RequestLogger`                 | Logs request method, path, and response status                                                                                                                       |
| `FnMiddleware`                  | Wraps a closure `Fn(RequestContext, Next<'_>) -> impl Future<Output = Result<Response, EdgeError>>`; build one with `middleware_fn(...)` or `FnMiddleware::new(...)` |

If you already hold an `Arc<dyn Middleware>` (`BoxMiddleware`), register it with
`.middleware_arc(...)` instead of `.middleware(...)`.

## Next Steps

- Learn about [Streaming](/guide/streaming) for progressive responses
- Explore [Proxying](/guide/proxying) for upstream forwarding
