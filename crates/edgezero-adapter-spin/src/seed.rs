//! Seed handler for `config push --adapter spin`.
//!
//! Provides a **host-compilable core** (`handle_seed_request_core`) and a
//! **wasm-gated wrapper** (`handle_seed_request_spin`) that translates Spin
//! request/response types to core ones. The split lets the security surface
//! (auth, token comparison, status-code routing, body parsing) be unit-
//! tested on the host without dragging in `spin_sdk` types.
//!
//! See `docs/superpowers/specs/2026-06-01-spin-kv-config.md` D9 / D10 for
//! the contract: status-code table, fail-closed token rules, 16-byte token
//! floor, body schema.

use async_trait::async_trait;
use edgezero_core::body::Body;
use edgezero_core::http::{header, response_builder, Method, Request, Response, StatusCode};
use serde::Deserialize;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use spin_sdk::http::Request as SpinRequest;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use spin_sdk::key_value::Store as SpinSdkKvStore;
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::{Mutex, PoisonError};
use subtle::ConstantTimeEq as _;
use thiserror::Error;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use crate::request::into_core_request;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use crate::response::from_core_response;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use crate::SpinFullResponse;

/// Minimum server-token length below which the handler is fail-closed
/// (returns 401 on every request). 16 bytes / 128 bits — kills practical
/// brute-force; placeholders like `"dev"` trip it immediately.
const MIN_TOKEN_LEN: usize = 16;

/// Fixed seed-handler route. Single canonical path per D9 — not configurable
/// per app, so ops scripts know exactly where to point.
pub(crate) const SEED_ROUTE: &str = "/__edgezero/config/seed";

/// Header carrying the seed token. Constant-time compared against the env-
/// resolved server token via `subtle::ConstantTimeEq`.
pub(crate) const SEED_TOKEN_HEADER: &str = "x-edgezero-seed";

#[derive(Debug, Error)]
pub(crate) enum SeedError {
    #[error("kv write failed for key `{key}`: {source}")]
    WriteFailed {
        key: String,
        #[source]
        source: anyhow::Error,
    },
}

#[async_trait(?Send)]
pub(crate) trait SeedWriter {
    /// Write a `(store, key, value)` tuple. Implementations should be infallible
    /// from a routing perspective; failures are surfaced as HTTP 422 by the
    /// caller and the failing key is named in the body.
    async fn write(&self, store: &str, key: &str, value: &str) -> Result<(), SeedError>;
}

/// Production wasm writer — opens the KV store fresh per write and calls
/// `set`. Lives behind the spin/wasm32 gate because `spin_sdk::key_value`
/// is a wasm hostcall.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub(crate) struct SpinKvSeedWriter;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[async_trait(?Send)]
impl SeedWriter for SpinKvSeedWriter {
    async fn write(&self, store: &str, key: &str, value: &str) -> Result<(), SeedError> {
        let kv = SpinSdkKvStore::open(store)
            .await
            .map_err(|err| SeedError::WriteFailed {
                key: key.to_owned(),
                source: anyhow::anyhow!("open `{store}`: {err}"),
            })?;
        kv.set(key, value.as_bytes())
            .await
            .map_err(|err| SeedError::WriteFailed {
                key: key.to_owned(),
                source: anyhow::anyhow!(err.to_string()),
            })
    }
}

#[cfg(test)]
pub(crate) struct InMemorySeedWriter {
    entries: Mutex<BTreeMap<(String, String), String>>,
    /// When true, the next `write` call returns `Err`. Used to test 422.
    fail_on_write: bool,
}

#[cfg(test)]
impl InMemorySeedWriter {
    pub(crate) fn failing() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            fail_on_write: true,
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            fail_on_write: false,
        }
    }

    pub(crate) fn recorded(&self) -> BTreeMap<(String, String), String> {
        // Recover from poisoning rather than panic — keeps restriction
        // lints (`expect_used` / `unwrap_used` / `panic`) clean and is
        // safe here since the inner map is recoverable state.
        let guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    }
}

#[cfg(test)]
#[async_trait(?Send)]
impl SeedWriter for InMemorySeedWriter {
    async fn write(&self, store: &str, key: &str, value: &str) -> Result<(), SeedError> {
        if self.fail_on_write {
            return Err(SeedError::WriteFailed {
                key: key.to_owned(),
                source: anyhow::anyhow!("forced write failure"),
            });
        }
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        guard.insert((store.to_owned(), key.to_owned()), value.to_owned());
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct SeedRequestBody {
    entries: Vec<SeedEntry>,
    store: String,
}

#[derive(Debug, Deserialize)]
struct SeedEntry {
    key: String,
    value: String,
}

#[expect(
    clippy::expect_used,
    reason = "response_builder() with a static StatusCode and Body::empty() is infallible — the only way it can fail is invalid header insertion, and we set none."
)]
fn empty_response(status: StatusCode) -> Response {
    response_builder()
        .status(status)
        .body(Body::empty())
        .expect("static status + empty body must build")
}

#[expect(
    clippy::expect_used,
    reason = "response_builder() with a static StatusCode + static header name/value + UTF-8 String body is infallible by construction."
)]
fn text_response(status: StatusCode, reason: impl Into<String>) -> Response {
    response_builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(reason.into()))
        .expect("static status + text body must build")
}

/// Apply the D9 fail-closed contract: returns `Some` only when the candidate
/// token is non-blank, non-whitespace-only, and at least [`MIN_TOKEN_LEN`]
/// bytes long. `None` triggers blanket 401 from the caller.
fn validated_server_token(raw: Option<&str>) -> Option<&str> {
    let token = raw?;
    if token.trim().is_empty() {
        return None;
    }
    if token.len() < MIN_TOKEN_LEN {
        return None;
    }
    Some(token)
}

/// Host-compilable seed handler core.
///
/// Routes the request through the D9 status-code table:
///
/// | Code | Condition |
/// |---|---|
/// | 204 | success |
/// | 400 | malformed JSON, missing `store`, empty `entries`, non-string values |
/// | 401 | header missing OR server token unset/blank/whitespace/<16 bytes |
/// | 403 | wire token does not match server token |
/// | 404 | `store` not in `known_platform_labels` |
/// | 405 | non-POST method |
/// | 415 | content-type not `application/json` |
/// | 422 | `SeedWriter::write` errored mid-stream |
///
/// `valid_token` is the env-resolved server token; `None`/blank/short triggers
/// fail-closed 401 (D9 "no token → no auth" rule).
///
/// `known_platform_labels` is the set of env-resolved platform labels the
/// caller computes from `A::stores().config × env.store_name("config", id)`
/// so the body's `store` can refer to the platform label (not the logical id).
pub(crate) async fn handle_seed_request_core<W: SeedWriter>(
    req: &Request,
    writer: &W,
    valid_token: Option<&str>,
    known_platform_labels: &[String],
) -> Response {
    // Method gate.
    if req.method() != Method::POST {
        return empty_response(StatusCode::METHOD_NOT_ALLOWED);
    }

    // Content-type gate. Accept `application/json` plus parameters
    // (`; charset=utf-8`) but nothing else.
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("application/json") {
        return empty_response(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    // Auth gate — fail-closed FIRST against the server token, then check
    // the wire token. Reversing the order would let a missing-token attacker
    // probe presence of a short server token.
    let Some(server_token) = validated_server_token(valid_token) else {
        return empty_response(StatusCode::UNAUTHORIZED);
    };
    let Some(wire_token) = req
        .headers()
        .get(SEED_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return empty_response(StatusCode::UNAUTHORIZED);
    };
    // Constant-time compare via subtle. `ct_eq` returns `Choice` (a u8-wrap)
    // which converts to `bool` infallibly.
    let eq: bool = wire_token.as_bytes().ct_eq(server_token.as_bytes()).into();
    if !eq {
        // Never log either token. Wire-token LENGTH is OK -- helps the
        // operator see "did I send the right shape" without leaking material.
        log::warn!(
            "seed handler: x-edgezero-seed mismatch (wire-token-len={})",
            wire_token.len()
        );
        return empty_response(StatusCode::FORBIDDEN);
    }

    // Body parse.
    let body_bytes = req.body().as_bytes().unwrap_or(&[]);
    let parsed: SeedRequestBody = match serde_json::from_slice(body_bytes) {
        Ok(parsed) => parsed,
        Err(err) => {
            return text_response(StatusCode::BAD_REQUEST, format!("malformed JSON: {err}"));
        }
    };
    if parsed.entries.is_empty() {
        return text_response(StatusCode::BAD_REQUEST, "entries must be non-empty");
    }

    // Store gate -- match the body's `store` against env-resolved platform
    // labels (NOT logical ids; see D9).
    if !known_platform_labels
        .iter()
        .any(|label| label == &parsed.store)
    {
        return text_response(
            StatusCode::NOT_FOUND,
            format!(
                "store `{}` is not a recognised platform label",
                parsed.store
            ),
        );
    }

    // Write entries sequentially. On first failure, surface 422 + the
    // failed key so the operator knows where the partial write stopped.
    for entry in &parsed.entries {
        if let Err(err) = writer.write(&parsed.store, &entry.key, &entry.value).await {
            return text_response(StatusCode::UNPROCESSABLE_ENTITY, err.to_string());
        }
    }
    empty_response(StatusCode::NO_CONTENT)
}

/// Thin wasm wrapper: Spin request → core request → core handler → core
/// response → Spin response. Lives behind the spin/wasm32 gate because
/// `into_core_request` and `from_core_response` are wasm-only.
///
/// Returns `anyhow::Result<SpinFullResponse>` to match `run_app`'s shape so
/// `run_app_with_seeder`'s `if/else` (seed branch vs fall-through) is
/// type-consistent without an `.expect()` panic.
///
/// # Errors
/// Propagates errors from `into_core_request` (malformed request line / body
/// read) and `from_core_response` (non-UTF-8 header values being smuggled in,
/// which can't happen with the static responses this handler emits but the
/// `?` keeps the surface symmetric).
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[inline]
pub(crate) async fn handle_seed_request_spin(
    req: SpinRequest,
    writer: &SpinKvSeedWriter,
    valid_token: Option<&str>,
    known_platform_labels: &[String],
) -> anyhow::Result<SpinFullResponse> {
    let core_req = into_core_request(req).await?;
    let core_resp =
        handle_seed_request_core(&core_req, writer, valid_token, known_platform_labels).await;
    Ok(from_core_response(core_resp).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::http::request_builder;
    use futures::executor::block_on;

    /// 21-byte token — exceeds the 16-byte floor.
    const VALID_TOKEN: &str = "test-token-1234567890";
    /// 15-byte token — just under the floor.
    const SHORT_TOKEN: &str = "tok-test-123456";
    /// Exactly 16 bytes — at the floor; valid.
    const AT_FLOOR_TOKEN: &str = "tok-test-1234567";

    fn labels() -> Vec<String> {
        vec!["app_config".to_owned()]
    }

    fn happy_body() -> Vec<u8> {
        br#"{"store":"app_config","entries":[{"key":"greeting","value":"hello"}]}"#.to_vec()
    }

    fn request_with(
        method: Method,
        content_type: &str,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> Request {
        let mut builder = request_builder()
            .method(method)
            .uri(SEED_ROUTE)
            .header(header::CONTENT_TYPE, content_type);
        if let Some(token_value) = token {
            builder = builder.header(SEED_TOKEN_HEADER, token_value);
        }
        builder.body(Body::from(body)).expect("static request")
    }

    fn post(token: Option<&str>, body: Vec<u8>) -> Request {
        request_with(Method::POST, "application/json", token, body)
    }

    #[test]
    fn server_token_unset_returns_401() {
        let req = post(Some(VALID_TOKEN), happy_body());
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(&req, &writer, None, &labels()));
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(writer.recorded().is_empty(), "no writes on auth failure");
    }

    #[test]
    fn server_token_blank_returns_401() {
        let req = post(Some(VALID_TOKEN), happy_body());
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(&req, &writer, Some(""), &labels()));
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn server_token_whitespace_returns_401() {
        let req = post(Some(VALID_TOKEN), happy_body());
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some("    "),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn server_token_15_bytes_returns_401_even_with_matching_wire() {
        assert_eq!(SHORT_TOKEN.len(), 15, "fixture invariant");
        // Client offers the same 15-byte token -- without the floor the
        // ct_eq would say "match" and serve. With the floor, server is
        // fail-closed so 401.
        let req = post(Some(SHORT_TOKEN), happy_body());
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(SHORT_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            writer.recorded().is_empty(),
            "fail-closed must short-circuit before any write"
        );
    }

    #[test]
    fn server_token_at_16_byte_floor_returns_204() {
        assert_eq!(AT_FLOOR_TOKEN.len(), 16, "fixture invariant");
        let req = post(Some(AT_FLOOR_TOKEN), happy_body());
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(AT_FLOOR_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(writer.recorded().len(), 1);
    }

    #[test]
    fn missing_wire_token_returns_401() {
        let req = post(None, happy_body());
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn wrong_wire_token_returns_403() {
        let req = post(Some("wrong-token-but-long-enough"), happy_body());
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn non_post_method_returns_405() {
        let req = request_with(
            Method::GET,
            "application/json",
            Some(VALID_TOKEN),
            happy_body(),
        );
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn non_json_content_type_returns_415() {
        let req = request_with(Method::POST, "text/plain", Some(VALID_TOKEN), happy_body());
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn malformed_json_returns_400() {
        let req = post(Some(VALID_TOKEN), b"{not-json".to_vec());
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn empty_entries_returns_400() {
        let body = br#"{"store":"app_config","entries":[]}"#.to_vec();
        let req = post(Some(VALID_TOKEN), body);
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn unknown_store_returns_404() {
        let body = br#"{"store":"surprise","entries":[{"key":"k","value":"v"}]}"#.to_vec();
        let req = post(Some(VALID_TOKEN), body);
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn writer_failure_returns_422() {
        let req = post(Some(VALID_TOKEN), happy_body());
        let writer = InMemorySeedWriter::failing();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn happy_path_returns_204_and_records_entries() {
        let body =
            br#"{"store":"app_config","entries":[{"key":"greeting","value":"hello"},{"key":"service.timeout_ms","value":"1500"}]}"#
                .to_vec();
        let req = post(Some(VALID_TOKEN), body);
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let recorded = writer.recorded();
        assert_eq!(recorded.len(), 2);
        assert_eq!(
            recorded.get(&("app_config".to_owned(), "greeting".to_owned())),
            Some(&"hello".to_owned()),
        );
        assert_eq!(
            recorded.get(&("app_config".to_owned(), "service.timeout_ms".to_owned())),
            Some(&"1500".to_owned()),
        );
    }
}
