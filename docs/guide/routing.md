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

You can also build routes programmatically:

```rust
use edgezero_core::router::RouterService;
use edgezero_core::http::Method;

let router = RouterService::builder()
    .route(Method::GET, "/hello", hello_handler)
    .route(Method::GET, "/echo/{name}", echo_handler)
    .route(Method::POST, "/echo", echo_json_handler)
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
router
    .route(Method::GET, "/resource", get_resource)
    .route(Method::POST, "/resource", create_resource)
    .route(Method::PUT, "/resource/{id}", update_resource)
    .route(Method::DELETE, "/resource/{id}", delete_resource)
```

EdgeZero automatically returns `405 Method Not Allowed` for requests that match a path but use an unsupported method.

## Route Listing

Enable route listing for debugging:

```rust
let router = RouterService::builder()
    .enable_route_listing()
    .route(Method::GET, "/hello", hello)
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

| Pattern | Example | Matches |
|---------|---------|---------|
| `/static` | `/static` | Exact match only |
| `/{param}` | `/users/{id}` | Single segment: `/users/123` |
| `/{*catch}` | `/files/{*path}` | Rest of path: `/files/a/b/c` |

::: warning Legacy Syntax
Axum-style `:name` parameters are **not supported**. Use `{name}` instead.
:::

## Route Priority

Routes are matched in registration order. More specific routes should be registered before catch-alls:

```rust
// Good: specific route first
router
    .route(Method::GET, "/users/me", get_current_user)
    .route(Method::GET, "/users/{id}", get_user_by_id)

// Bad: catch-all shadows specific routes
router
    .route(Method::GET, "/users/{id}", get_user_by_id)
    .route(Method::GET, "/users/me", get_current_user)  // Never reached!
```

## Next Steps

- Learn about [Handlers & Extractors](/guide/handlers) for processing requests
- Explore [Middleware](/guide/middleware) for cross-cutting concerns
