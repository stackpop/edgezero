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

### Request Context

Access the full request context for headers, method, URI, etc:

```rust
use edgezero_core::context::RequestContext;

#[action]
async fn inspect(RequestContext(ctx): RequestContext) -> Text<String> {
    let method = ctx.method();
    let path = ctx.uri().path();
    let user_agent = ctx.headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    
    Text::new(format!("{} {} from {}", method, path, user_agent))
}
```

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

```rust
use edgezero_core::response::Json;

#[derive(serde::Serialize)]
struct User {
    id: u64,
    name: String,
}

#[action]
async fn get_user() -> Json<User> {
    Json(User { id: 1, name: "Alice".into() })
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
use edgezero_core::http::{HeaderMap, HeaderValue};
use edgezero_core::response::Text;

#[action]
async fn with_headers() -> (HeaderMap, Text<&'static str>) {
    let mut headers = HeaderMap::new();
    headers.insert("x-custom", HeaderValue::from_static("value"));
    (headers, Text::new("Response with custom header"))
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
) -> Json<User> {
    // All three extractors are available
    Json(User { id, name: body.name })
}
```

## Error Handling

Extractors return `EdgeError` on failure, which automatically converts to appropriate HTTP responses:

| Error | Status Code |
|-------|-------------|
| JSON parse error | 400 Bad Request |
| Validation error | 400 Bad Request |
| Missing path param | 500 Internal Server Error |
| Type conversion error | 400 Bad Request |

For custom error handling, return `Result`:

```rust
use edgezero_core::error::EdgeError;

#[action]
async fn fallible(Json(body): Json<Request>) -> Result<Json<Response>, EdgeError> {
    if body.invalid {
        return Err(EdgeError::bad_request("Invalid request"));
    }
    Ok(Json(Response { success: true }))
}
```

## Next Steps

- Learn about [Middleware](/guide/middleware) for request/response processing
- Explore [Streaming](/guide/streaming) for large response bodies
