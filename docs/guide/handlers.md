# Handlers & Extractors

EdgeZero provides ergonomic handler definitions using the `#[action]` macro and type-safe extractors.

## The #[action] Macro

The `#[action]` macro transforms async functions into EdgeZero handlers with automatic extractor wiring:

```rust
use edgezero_core::action;
use edgezero_core::extractor::Json;
use edgezero_core::response::Text;

#[derive(serde::Deserialize)]
struct CreateUser {
    name: String,
    email: String,
}

#[action]
async fn create_user(Json(body): Json<CreateUser>) -> Text<String> {
    Text::new(format!("Created user: {}", body.name))
}
```

The macro:

- Generates the `FromRequest` boilerplate for each extractor
- Handles async execution
- Converts the return type into a proper response

## Built-in Extractors

### Path Parameters

Extract typed parameters from the URL path:

```rust
use edgezero_core::extractor::Path;

// Single parameter
#[action]
async fn get_user(Path(id): Path<u64>) -> Text<String> {
    Text::new(format!("User ID: {}", id))
}

// Multiple parameters via struct
#[derive(serde::Deserialize)]
struct PostPath {
    user_id: u64,
    post_id: u64,
}

#[action]
async fn get_post(Path(params): Path<PostPath>) -> Text<String> {
    Text::new(format!("User {} Post {}", params.user_id, params.post_id))
}
```

### Query Parameters

Extract query string parameters:

```rust
use edgezero_core::extractor::Query;

#[derive(serde::Deserialize)]
struct Pagination {
    page: Option<u32>,
    limit: Option<u32>,
}

#[action]
async fn list_items(Query(params): Query<Pagination>) -> Text<String> {
    let page = params.page.unwrap_or(1);
    let limit = params.limit.unwrap_or(10);
    Text::new(format!("Page {} with {} items", page, limit))
}
```

### JSON Body

Parse JSON request bodies:

```rust
use edgezero_core::extractor::Json;

#[derive(serde::Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[action]
async fn login(Json(body): Json<LoginRequest>) -> Text<String> {
    Text::new(format!("Logging in: {}", body.username))
}
```

### Validated Extractors

Use `validator` crate integration for input validation:

```rust
use edgezero_core::extractor::{ValidatedJson, ValidatedQuery};
use validator::Validate;

#[derive(serde::Deserialize, Validate)]
struct CreatePost {
    #[validate(length(min = 1, max = 200))]
    title: String,
    #[validate(length(min = 1))]
    content: String,
}

#[action]
async fn create_post(ValidatedJson(body): ValidatedJson<CreatePost>) -> Text<String> {
    Text::new(format!("Created post: {}", body.title))
}
```

If validation fails, EdgeZero automatically returns a 400 Bad Request with error details.

### Headers

Extract request headers directly:

```rust
use edgezero_core::extractor::Headers;

#[action]
async fn check_auth(Headers(headers): Headers) -> Text<String> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("none");
    Text::new(format!("Auth: {}", token))
}
```

### Form Data

Parse URL-encoded form bodies:

```rust
use edgezero_core::extractor::Form;

#[derive(serde::Deserialize)]
struct ContactForm {
    name: String,
    email: String,
}

#[action]
async fn submit_form(Form(data): Form<ContactForm>) -> Text<String> {
    Text::new(format!("Received from: {}", data.email))
}
```

Use `ValidatedForm<T>` for form data with validation, and `ValidatedPath<T>` for validated path parameters.

### Host Extractors

Extract the hostname from request headers:

```rust
use edgezero_core::extractor::{Host, ForwardedHost};

// Extract from the Host header (falls back to "localhost")
#[action]
async fn check_host(Host(host): Host) -> Text<String> {
    Text::new(format!("Host: {}", host))
}

// Extract from X-Forwarded-Host first, then Host header
// Use this when behind a reverse proxy or load balancer
#[action]
async fn check_forwarded(ForwardedHost(host): ForwardedHost) -> Text<String> {
    Text::new(format!("Effective host: {}", host))
}
```

### Request Context

For full request access, handlers can receive `RequestContext` directly (no `#[action]` needed):

```rust
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;

async fn inspect(ctx: RequestContext) -> Result<Text<String>, EdgeError> {
    let method = ctx.request().method();
    let path = ctx.request().uri().path();
    let user_agent = ctx.request().headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");

    Ok(Text::new(format!("{} {} from {}", method, path, user_agent)))
}
```

`RequestContext` provides these methods:

| Method           | Returns                                    |
| ---------------- | ------------------------------------------ |
| `request()`      | `&Request` - full HTTP request             |
| `path_params()`  | `&PathParams` - raw path parameters        |
| `path::<T>()`    | Deserialize path params to `T`             |
| `query::<T>()`   | Deserialize query string to `T`            |
| `json::<T>()`    | Deserialize JSON body to `T`               |
| `form::<T>()`    | Deserialize form body to `T`               |
| `body()`         | `&Body` - raw request body                 |
| `into_request()` | `Request` - consume context, take request  |
| `proxy_handle()` | `Option<ProxyHandle>` - adapter proxy hook |

## Sharing app state

Request-derived extractors (`Json`, `Query`, `Path`, …) cover per-request data.
For app-owned state that outlives a single request — a settings object, a
connection registry, an orchestrator — register it once on the router and read
it back with the `State<T>` extractor.

Register the value with `RouterBuilder::with_state`. It is cloned into every
request's extensions before dispatch, so `T` must be `Clone + Send + Sync +
'static` — typically an `Arc<AppState>`, where the clone is a cheap refcount
bump:

```rust
use std::sync::Arc;
use edgezero_core::extractor::State;
use edgezero_core::router::RouterService;

#[derive(Clone)]
struct AppState {
    greeting: String,
}

let state = Arc::new(AppState { greeting: "hello".into() });

let service = RouterService::builder()
    .with_state(Arc::clone(&state))
    .get("/greet", greet)
    .build();
```

Read it in any `#[action]` handler by adding a `State<T>` argument — it composes
with the other extractors:

```rust
use edgezero_core::action;
use edgezero_core::error::EdgeError;
use edgezero_core::extractor::State;
use std::sync::Arc;

#[action]
async fn greet(
    State(state): State<Arc<AppState>>,
) -> Result<String, EdgeError> {
    Ok(state.greeting.clone())
}
```

Register different types independently (`with_state(a).with_state(b)`); each is
resolved by its own type. Registering the same `T` twice is last-write-wins. If
a handler asks for a `State<T>` that was never registered, extraction fails with
a `500` — register it before `build()`.

### With the `app!` macro

If your app is fully macro-driven (`app!("edgezero.toml")` builds the router from
the manifest), you don't hand-write `RouterBuilder::with_state`. Instead pass a
`state` argument — the macro emits `.with_state(<expr>)` into the generated
router:

```rust
// lib.rs
use std::sync::{Arc, OnceLock};

pub struct AppState { /* settings, registries, orchestrator, … */ }

pub fn app_state() -> Arc<AppState> {
    static STATE: OnceLock<Arc<AppState>> = OnceLock::new();
    Arc::clone(STATE.get_or_init(|| Arc::new(AppState { /* … */ })))
}

edgezero_core::app!("edgezero.toml", state = crate::app_state());
```

`state = <expr>` is any expression evaluating to the state value — write the call
(`crate::app_state()`), not a bare function path. Handlers then extract
`State<Arc<AppState>>` exactly as above. Only **one** `state = <expr>` is
allowed per `app!` — to share more than one value, wrap them in an aggregate
app-state struct and register that.

> **Make `app_state()` cheap.** The macro emits the `state` expression inside the
> generated `build_router()`, which each adapter's `run_app` calls through
> `A::build_app()` — **once at startup** for long-lived runtimes (Axum), but
> **once per request** on Fastly Compute (each request is a fresh Wasm instance).
> So the `state` expression runs on that cadence: build heavy state **once** and
> hand out clones (e.g. a `OnceLock<Arc<AppState>>` as above, or a `static`), and
> let `T = Arc<AppState>` so each call is just a refcount bump. Do **not** `Arc::new(HeavyThing::build())` directly in the `state` expression.

## Response Types

### Text Responses

```rust
use edgezero_core::response::Text;

#[action]
async fn hello() -> Text<&'static str> {
    Text::new("Hello, World!")
}
```

### JSON Responses

Build JSON responses using `Body::json`:

```rust
use edgezero_core::body::Body;
use edgezero_core::http::{Response, StatusCode};
use edgezero_core::error::EdgeError;

#[derive(serde::Serialize)]
struct User {
    id: u64,
    name: String,
}

#[action]
async fn get_user() -> Result<Response, EdgeError> {
    let user = User { id: 1, name: "Alice".into() };
    let body = Body::json(&user).map_err(EdgeError::internal)?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(body)
        .unwrap())
}
```

### Status Codes

```rust
use edgezero_core::http::StatusCode;
use edgezero_core::response::Text;

#[action]
async fn not_found() -> (StatusCode, Text<&'static str>) {
    (StatusCode::NOT_FOUND, Text::new("Resource not found"))
}
```

### Custom Headers

```rust
use edgezero_core::body::Body;
use edgezero_core::http::{HeaderValue, Response, StatusCode};

#[action]
async fn with_headers() -> Response {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("Response with custom header"))
        .unwrap();
    response
        .headers_mut()
        .insert("x-custom", HeaderValue::from_static("value"));
    response
}
```

## Combining Extractors

You can use multiple extractors in a single handler:

```rust
#[action]
async fn update_user(
    Path(id): Path<u64>,
    Query(params): Query<UpdateOptions>,
    Json(body): Json<UpdateUser>,
) -> Text<String> {
    Text::new(format!("Updated user {} with name {}", id, body.name))
}
```

## Error Handling

Extractors return `EdgeError` on failure, which automatically converts to appropriate HTTP responses:

| Error                 | Status Code              |
| --------------------- | ------------------------ |
| JSON parse error      | 400 Bad Request          |
| Validation error      | 422 Unprocessable Entity |
| Missing path param    | 400 Bad Request          |
| Type conversion error | 400 Bad Request          |

For custom error handling, return `Result`:

```rust
use edgezero_core::error::EdgeError;

#[action]
async fn fallible(Json(body): Json<MyRequest>) -> Result<Text<String>, EdgeError> {
    if body.invalid {
        return Err(EdgeError::bad_request("Invalid request"));
    }
    Ok(Text::new("Success"))
}
```

### EdgeError Methods

`EdgeError` provides factory methods for common HTTP errors:

```rust
use edgezero_core::error::EdgeError;

// Client errors
EdgeError::bad_request("Invalid input")           // 400
EdgeError::not_found("/missing/path")             // 404
EdgeError::method_not_allowed(&method, &allowed)  // 405
EdgeError::validation("Field too short")          // 422

// Server errors
EdgeError::internal("Unexpected failure")         // 500
EdgeError::internal(some_error)                   // 500 (from any error type)
```

## Custom Extractors

Implement the `FromRequest` trait to create custom extractors:

```rust
use async_trait::async_trait;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::extractor::FromRequest;

pub struct BearerToken(pub String);

#[async_trait(?Send)]
impl FromRequest for BearerToken {
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let header = ctx.request().headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| EdgeError::bad_request("Missing Authorization header"))?;

        let token = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| EdgeError::bad_request("Invalid Bearer token format"))?;

        Ok(BearerToken(token.to_string()))
    }
}

// Use in handlers:
#[action]
async fn protected(BearerToken(token): BearerToken) -> Text<String> {
    Text::new(format!("Authenticated with token: {}...", &token[..8]))
}
```

## Custom Response Types

Implement `IntoResponse` for custom response types:

```rust
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Response, StatusCode};
use edgezero_core::response::IntoResponse;

pub struct HtmlResponse(pub String);

impl IntoResponse for HtmlResponse {
    fn into_response(self) -> Result<Response, EdgeError> {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/html; charset=utf-8")
            .body(Body::from(self.0))
            .map_err(EdgeError::internal)
    }
}

// Use in handlers:
#[action]
async fn page() -> HtmlResponse {
    HtmlResponse("<h1>Hello</h1>".to_string())
}
```

## Next Steps

- Learn about [Middleware](/guide/middleware) for request/response processing
- Explore [Streaming](/guide/streaming) for large response bodies
