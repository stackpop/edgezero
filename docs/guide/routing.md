# Routing

EdgeZero uses `matchit` 0.8+ for high-performance path matching with support for parameters and catch-all segments.

## Defining Routes

Routes are typically defined in your `edgezero.toml` manifest and wired automatically via the `app!` macro:

```toml
[[triggers.http]]
id = "hello"
path = "/hello"
methods = ["GET"]
handler = "my_app_core::handlers::hello"

[[triggers.http]]
id = "echo"
path = "/echo/{name}"
methods = ["GET", "POST"]
handler = "my_app_core::handlers::echo"
```

You can also build routes programmatically using convenience methods:

```rust
use edgezero_core::router::RouterService;

let router = RouterService::builder()
    .get("/hello", hello_handler)
    .get("/echo/{name}", echo_handler)
    .post("/echo", echo_json_handler)
    .build();
```

Or with explicit method specification:

```rust
use edgezero_core::router::RouterService;
use edgezero_core::http::Method;

let router = RouterService::builder()
    .route("/hello", Method::GET, hello_handler)
    .route("/echo/{name}", Method::GET, echo_handler)
    .route("/echo", Method::POST, echo_json_handler)
    .build();
```

## Path Parameters

Define parameters with `{name}` segments:

```rust
use edgezero_core::action;
use edgezero_core::extractor::Path;
use edgezero_core::response::Text;

#[action]
async fn greet(Path(name): Path<String>) -> Text<String> {
    Text::new(format!("Hello, {}!", name))
}
```

For routes like `/users/{id}/posts/{post_id}`, extract multiple parameters:

```rust
#[derive(serde::Deserialize)]
struct PostParams {
    id: u64,
    post_id: u64,
}

#[action]
async fn get_post(Path(params): Path<PostParams>) -> Text<String> {
    Text::new(format!("User {} Post {}", params.id, params.post_id))
}
```

## Catch-All Segments

Use `{*rest}` for catch-all routes that match any remaining path:

```rust
// Route: /files/{*path}
// Matches: /files/docs/readme.md -> path = "docs/readme.md"

#[action]
async fn serve_file(Path(path): Path<String>) -> Text<String> {
    Text::new(format!("Serving: {}", path))
}
```

## HTTP Methods

Specify allowed methods in your route definition:

```toml
[[triggers.http]]
path = "/resource"
methods = ["GET", "POST", "PUT", "DELETE"]
handler = "my_app_core::handlers::resource"
```

Or programmatically:

```rust
RouterService::builder()
    .get("/resource", get_resource)
    .post("/resource", create_resource)
    .put("/resource/{id}", update_resource)
    .delete("/resource/{id}", delete_resource)
    .build()
```

EdgeZero automatically returns `405 Method Not Allowed` for requests that match a path but use an unsupported method.

## Route Listing

Enable route listing for debugging:

```rust
let router = RouterService::builder()
    .enable_route_listing()
    .get("/hello", hello)
    .build();
```

This exposes a JSON endpoint at `/__edgezero/routes`:

```json
[
  { "method": "GET", "path": "/hello" },
  { "method": "GET", "path": "/__edgezero/routes" }
]
```

Customize the listing path:

```rust
RouterService::builder()
    .enable_route_listing_at("/debug/routes")
```

## Path Syntax

EdgeZero uses matchit's path syntax:

| Pattern     | Example          | Matches                      |
| ----------- | ---------------- | ---------------------------- |
| `/static`   | `/static`        | Exact match only             |
| `/{param}`  | `/users/{id}`    | Single segment: `/users/123` |
| `/{*catch}` | `/files/{*path}` | Rest of path: `/files/a/b/c` |

::: warning Legacy Syntax
Axum-style `:name` parameters are **not supported**. Use `{name}` instead.
:::

## Route Priority

Routes are matched by specificity (static segments first, then parameters, then catch-alls). If two
routes have the same specificity, the first registered wins. Avoid ambiguous patterns that share
the same shape (for example, two routes that both look like `/users/{id}`).

## Next Steps

- Learn about [Handlers & Extractors](/guide/handlers) for processing requests
- Explore [Middleware](/guide/middleware) for cross-cutting concerns
