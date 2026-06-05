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
/// (returns 401 on every request). 16 *bytes* (not characters) — kills
/// practical brute-force at 128 bits when the token is 16 random bytes;
/// placeholders like `"dev"` trip it immediately. Note that hex-encoded
/// tokens have half the entropy per byte, so operators should use raw
/// random bytes (e.g. `openssl rand -base64 16`) or pick longer hex
/// strings (32+ chars).
const MIN_TOKEN_LEN: usize = 16;

/// Maximum request-body size before parsing — bounds the pre-auth read
/// surface so an unauthenticated attacker can't OOM a Spin instance with
/// a multi-MB POST. 256 KiB comfortably fits the typed config-flattened
/// payload for any reasonable app.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Maximum entries per push. Each entry is a sequential `kv.set`; capping
/// limits a stuck or malicious push from monopolising the runtime.
const MAX_ENTRIES: usize = 1000;

/// Maximum bytes per entry value. Spin KV is intended for small config
/// values, not blobs.
const MAX_VALUE_BYTES: usize = 64 * 1024;

/// Fixed seed-handler route. Single canonical path per D9 — not configurable
/// per app, so ops scripts know exactly where to point.
pub(crate) const SEED_ROUTE: &str = "/__edgezero/config/seed";

/// Header carrying the seed token. Constant-time compared against the env-
/// resolved server token via `subtle::ConstantTimeEq`.
pub(crate) const SEED_TOKEN_HEADER: &str = "x-edgezero-seed";

#[derive(Debug, Error)]
pub(crate) enum SeedError {
    /// The named platform store does not exist or can't be opened — distinct
    /// from a transient write failure so the seed handler can map it to 404
    /// (operator declared a label the runtime doesn't know about) instead of
    /// blanket 422.
    #[error("no such store `{store}`: {source}")]
    NoSuchStore {
        store: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("kv write failed for key `{key}` at entry {index}: {source}")]
    WriteFailed {
        index: usize,
        key: String,
        #[source]
        source: anyhow::Error,
    },
}

#[async_trait(?Send)]
pub(crate) trait SeedWriter {
    /// Open `store` once and write every `(key, value)` in `entries`. The
    /// store-open is hoisted out of the per-entry loop so an N-entry batch
    /// costs one KV `Store::open` (not N). On the first per-entry failure,
    /// returns the failing entry's index in the original `entries` slice
    /// (via `SeedError::WriteFailed.index`) so the seed handler's 422 body
    /// can name the offset — operators can trim earlier entries and retry
    /// without re-writing committed prefixes.
    async fn write_batch(&self, store: &str, entries: &[(&str, &str)]) -> Result<(), SeedError>;
}

/// Production wasm writer — opens the KV store once and calls `set` per
/// entry. Lives behind the spin/wasm32 gate because `spin_sdk::key_value`
/// is a wasm hostcall.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub(crate) struct SpinKvSeedWriter;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[async_trait(?Send)]
impl SeedWriter for SpinKvSeedWriter {
    async fn write_batch(&self, store: &str, entries: &[(&str, &str)]) -> Result<(), SeedError> {
        let kv = SpinSdkKvStore::open(store).await.map_err(|err| {
            // Spin's `key_value::Error` does not currently expose a
            // discriminant we can match on without coupling to the SDK's
            // internal repr; treat any open failure as "no such store"
            // since the seed handler has already vetted the label set.
            SeedError::NoSuchStore {
                store: store.to_owned(),
                source: anyhow::anyhow!(err.to_string()),
            }
        })?;
        for (index, (key, value)) in entries.iter().enumerate() {
            kv.set(key, value.as_bytes())
                .await
                .map_err(|err| SeedError::WriteFailed {
                    index,
                    key: (*key).to_owned(),
                    source: anyhow::anyhow!(err.to_string()),
                })?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) enum InMemorySeedMode {
    /// Fail with `SeedError::WriteFailed` at the given entry index. `0`
    /// is the original "fail on the first write" semantics.
    FailWriteAt(usize),
    /// Fail with `SeedError::NoSuchStore` before any write — mirrors the
    /// runtime's "label not declared" path so the seed handler's 404
    /// mapping can be exercised on host.
    NoSuchStore,
    /// Succeed on every entry.
    Ok,
}

#[cfg(test)]
pub(crate) struct InMemorySeedWriter {
    entries: Mutex<BTreeMap<(String, String), String>>,
    mode: InMemorySeedMode,
}

#[cfg(test)]
impl InMemorySeedWriter {
    pub(crate) fn failing() -> Self {
        Self::with_mode(InMemorySeedMode::FailWriteAt(0))
    }

    pub(crate) fn failing_at(index: usize) -> Self {
        Self::with_mode(InMemorySeedMode::FailWriteAt(index))
    }

    pub(crate) fn new() -> Self {
        Self::with_mode(InMemorySeedMode::Ok)
    }

    pub(crate) fn no_such_store() -> Self {
        Self::with_mode(InMemorySeedMode::NoSuchStore)
    }

    pub(crate) fn recorded(&self) -> BTreeMap<(String, String), String> {
        // Recover from poisoning rather than panic — keeps restriction
        // lints (`expect_used` / `unwrap_used` / `panic`) clean and is
        // safe here since the inner map is recoverable state.
        let guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    }

    fn with_mode(mode: InMemorySeedMode) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            mode,
        }
    }
}

#[cfg(test)]
#[async_trait(?Send)]
impl SeedWriter for InMemorySeedWriter {
    async fn write_batch(&self, store: &str, entries: &[(&str, &str)]) -> Result<(), SeedError> {
        if matches!(self.mode, InMemorySeedMode::NoSuchStore) {
            return Err(SeedError::NoSuchStore {
                store: store.to_owned(),
                source: anyhow::anyhow!("forced no-such-store failure"),
            });
        }
        for (index, (key, value)) in entries.iter().enumerate() {
            if matches!(self.mode, InMemorySeedMode::FailWriteAt(target) if target == index) {
                return Err(SeedError::WriteFailed {
                    index,
                    key: (*key).to_owned(),
                    source: anyhow::anyhow!("forced write failure at index {index}"),
                });
            }
            let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            guard.insert((store.to_owned(), (*key).to_owned()), (*value).to_owned());
        }
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
/// Routes the request through the D9 status-code table. Gates are
/// ordered fail-closed-FIRST: an unset / blank / whitespace / short
/// server token returns 401 on EVERY request (including GET and the
/// wrong content-type), matching the `run_app_with_seeder` docstring
/// contract. Without the token-first ordering an unauthenticated
/// caller could fingerprint method/content-type behaviour before auth
/// is enforced.
///
/// | Code | Condition |
/// |---|---|
/// | 204 | success |
/// | 400 | malformed JSON, missing `store`, empty `entries`, non-string values |
/// | 401 | server token unset/blank/whitespace/<16 bytes, OR wire-token header missing |
/// | 403 | wire token does not match server token |
/// | 404 | `store` not in `known_platform_labels`, or runtime reports no such store |
/// | 405 | non-POST method (only checked when auth succeeded) |
/// | 413 | request body exceeds `MAX_BODY_BYTES`, `entries.len()` exceeds `MAX_ENTRIES`, or any `value.len()` exceeds `MAX_VALUE_BYTES` |
/// | 415 | content-type not `application/json` (only checked when auth succeeded) |
/// | 422 | `SeedWriter::write_batch` errored mid-stream (body names failing index + key) |
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
    // Auth gate FIRST — fail-closed against the server token regardless of
    // method/content-type. This matches the `run_app_with_seeder`
    // contract ("every seed-route request returns 401 when the token is
    // unset/blank/short") and prevents pre-auth fingerprinting.
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
    // which converts to `bool` infallibly. `subtle` short-circuits on
    // length-mismatch, leaking server-token length via timing — acceptable
    // because the 16-byte floor still gives 128 bits of search space, and
    // we deliberately do NOT log the wire-token length on mismatch (would
    // be a second oracle for the same fact).
    let eq: bool = wire_token.as_bytes().ct_eq(server_token.as_bytes()).into();
    if !eq {
        // Never log either token, and never log either token's length —
        // see comment above the ct_eq call.
        log::warn!("seed handler: x-edgezero-seed mismatch");
        return empty_response(StatusCode::FORBIDDEN);
    }

    // Method gate (post-auth).
    if req.method() != Method::POST {
        return empty_response(StatusCode::METHOD_NOT_ALLOWED);
    }

    // Content-type gate (post-auth). Accept `application/json` with no
    // parameters, OR `application/json` followed by `;` and parameters
    // (`; charset=utf-8`). Plain `starts_with("application/json")` would
    // also accept `application/json-bad`, which is the wrong shape.
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if content_type != "application/json" && !content_type.starts_with("application/json;") {
        return empty_response(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    // Body size cap — pre-parse. Bounds the read surface so an
    // authenticated-but-malicious push (and a buggy operator script)
    // can't wedge the runtime with a multi-MB payload.
    let body_bytes = req.body().as_bytes().unwrap_or(&[]);
    if body_bytes.len() > MAX_BODY_BYTES {
        return text_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "request body exceeds {MAX_BODY_BYTES} bytes (got {})",
                body_bytes.len()
            ),
        );
    }

    // Body parse.
    let parsed: SeedRequestBody = match serde_json::from_slice(body_bytes) {
        Ok(parsed) => parsed,
        Err(err) => {
            return text_response(StatusCode::BAD_REQUEST, format!("malformed JSON: {err}"));
        }
    };
    if parsed.entries.is_empty() {
        return text_response(StatusCode::BAD_REQUEST, "entries must be non-empty");
    }
    if parsed.entries.len() > MAX_ENTRIES {
        return text_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "entries.len() = {} exceeds the {MAX_ENTRIES}-entry cap",
                parsed.entries.len()
            ),
        );
    }
    if let Some(oversized) = parsed
        .entries
        .iter()
        .enumerate()
        .find(|(_, entry)| entry.value.len() > MAX_VALUE_BYTES)
    {
        let (index, entry) = oversized;
        return text_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "entries[{index}].value (key `{}`) is {} bytes, exceeds the {MAX_VALUE_BYTES}-byte per-value cap",
                entry.key,
                entry.value.len()
            ),
        );
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

    // Hoist the (key, value) borrow set out of the body — `write_batch`
    // takes `&[(&str, &str)]` so the impl can open the store once.
    let entry_refs: Vec<(&str, &str)> = parsed
        .entries
        .iter()
        .map(|entry| (entry.key.as_str(), entry.value.as_str()))
        .collect();

    match writer.write_batch(&parsed.store, &entry_refs).await {
        Ok(()) => empty_response(StatusCode::NO_CONTENT),
        Err(SeedError::NoSuchStore { store, source }) => text_response(
            StatusCode::NOT_FOUND,
            format!("runtime rejected store `{store}`: {source}. did you declare the label in spin.toml's `key_value_stores` AND register a backend for it in runtime-config.toml?"),
        ),
        Err(err @ SeedError::WriteFailed { .. }) => {
            // err's Display already names the index + key.
            text_response(StatusCode::UNPROCESSABLE_ENTITY, err.to_string())
        }
    }
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
    use std::fmt::Write as _;
    use std::str;

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

    /// Runtime reports the label isn't backed by any registered KV
    /// store (the operator added it to `key_value_stores` but forgot
    /// the `runtime-config.toml` stanza). The seed handler already
    /// vetted the body label against `known_platform_labels`, so this
    /// is a configuration drift error — map to 404 with an actionable
    /// hint instead of blanket 422.
    #[test]
    fn writer_no_such_store_returns_404_naming_store_and_runtime_config() {
        let req = post(Some(VALID_TOKEN), happy_body());
        let writer = InMemorySeedWriter::no_such_store();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body_text = str::from_utf8(resp.body().as_bytes().expect("test response body is Once"))
            .expect("404 body is utf-8");
        assert!(
            body_text.contains("app_config") && body_text.contains("runtime-config.toml"),
            "404 body names the store + points at runtime-config.toml: {body_text}"
        );
        assert!(writer.recorded().is_empty(), "no writes on no-such-store");
    }

    /// 422 body must name BOTH the failing entry index AND the key so
    /// the operator can trim earlier entries and retry without
    /// re-writing committed prefixes.
    #[test]
    fn writer_failure_at_index_one_returns_422_naming_index_and_key() {
        // Two-entry body — fail on the 2nd write so the first entry was
        // already committed in the in-memory store. The 422 body must
        // name `index = 1` and `key = "second"`.
        let body = br#"{"store":"app_config","entries":[{"key":"first","value":"a"},{"key":"second","value":"b"}]}"#.to_vec();
        let req = post(Some(VALID_TOKEN), body);
        let writer = InMemorySeedWriter::failing_at(1);
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body_text = str::from_utf8(resp.body().as_bytes().expect("test response body is Once"))
            .expect("422 body is utf-8");
        assert!(
            body_text.contains("entry 1") && body_text.contains("`second`"),
            "422 body must name failing index + key: {body_text}"
        );
        // The first entry's write committed before the second failed.
        let recorded = writer.recorded();
        assert_eq!(
            recorded.len(),
            1,
            "partial-write semantics: first entry stuck"
        );
        assert!(recorded.contains_key(&("app_config".to_owned(), "first".to_owned())));
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

    // ---------- fail-closed ordering (token gate FIRST) ----------

    /// `run_app_with_seeder`'s docstring promises that every seed-route
    /// request returns 401 when the server token is unset/blank/short.
    /// A GET with no server token must trip 401 NOT 405 — otherwise an
    /// unauthenticated attacker can fingerprint the route as a seed
    /// handler by observing method-gate behaviour.
    #[test]
    fn get_with_unset_server_token_returns_401_not_405() {
        let req = request_with(Method::GET, "application/json", Some(VALID_TOKEN), vec![]);
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(&req, &writer, None, &labels()));
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "fail-closed token gate must fire before the method gate"
        );
    }

    /// Same fail-closed promise — wrong content-type with no server
    /// token must trip 401, not 415.
    #[test]
    fn wrong_content_type_with_unset_server_token_returns_401_not_415() {
        let req = request_with(Method::POST, "text/plain", Some(VALID_TOKEN), vec![]);
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(&req, &writer, None, &labels()));
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "fail-closed token gate must fire before the content-type gate"
        );
    }

    // ---------- body / entries / value caps ----------

    /// Pre-auth attack surface bound — a body over `MAX_BODY_BYTES` is
    /// rejected with 413 before `serde_json::from_slice` runs, so an
    /// authenticated push can't OOM the runtime with a multi-MB POST.
    #[test]
    fn body_over_max_size_returns_413() {
        let bloat = "x".repeat(MAX_BODY_BYTES + 1);
        let body =
            format!(r#"{{"store":"app_config","entries":[{{"key":"k","value":"{bloat}"}}]}}"#)
                .into_bytes();
        assert!(body.len() > MAX_BODY_BYTES);
        let req = post(Some(VALID_TOKEN), body);
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// `MAX_ENTRIES` + 1 entries -> 413 with a body that names the cap.
    /// Use a programmatically-built body to avoid bloating the test
    /// source; we don't need real entry contents, just count.
    #[test]
    fn entries_over_cap_returns_413() {
        let mut entries = String::with_capacity(MAX_ENTRIES * 20);
        for i in 0..=MAX_ENTRIES {
            if i > 0 {
                entries.push(',');
            }
            write!(entries, r#"{{"key":"k{i}","value":"v"}}"#)
                .expect("write to String is infallible");
        }
        let body = format!(r#"{{"store":"app_config","entries":[{entries}]}}"#).into_bytes();
        // Body itself stays well under MAX_BODY_BYTES (~25 KB at 1001
        // entries) so the entries-cap path is exercised, not the body
        // cap.
        assert!(body.len() < MAX_BODY_BYTES);
        let req = post(Some(VALID_TOKEN), body);
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body_text = str::from_utf8(resp.body().as_bytes().expect("test response body is Once"))
            .expect("413 body is utf-8");
        assert!(
            body_text.contains(&MAX_ENTRIES.to_string()),
            "413 body names the cap: {body_text}"
        );
    }

    /// A single entry whose value exceeds `MAX_VALUE_BYTES` -> 413 with a
    /// body that names the offending entry index AND key.
    #[test]
    fn value_over_per_value_cap_returns_413_naming_index_and_key() {
        let oversized = "x".repeat(MAX_VALUE_BYTES + 1);
        let body = format!(r#"{{"store":"app_config","entries":[{{"key":"a","value":"ok"}},{{"key":"big","value":"{oversized}"}}]}}"#).into_bytes();
        let req = post(Some(VALID_TOKEN), body);
        let writer = InMemorySeedWriter::new();
        let resp = block_on(handle_seed_request_core(
            &req,
            &writer,
            Some(VALID_TOKEN),
            &labels(),
        ));
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body_text = str::from_utf8(resp.body().as_bytes().expect("test response body is Once"))
            .expect("413 body is utf-8");
        assert!(
            body_text.contains("entries[1]") && body_text.contains("`big`"),
            "413 body names oversized index + key: {body_text}"
        );
        assert!(
            writer.recorded().is_empty(),
            "value-cap rejection must fire BEFORE any write"
        );
    }

    // ---------- content-type tightening (N-L1) ----------

    /// `application/json-bad` is NOT a JSON media type — the previous
    /// `starts_with("application/json")` check accepted it.
    #[test]
    fn content_type_application_json_bad_returns_415() {
        let req = request_with(
            Method::POST,
            "application/json-bad",
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
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    /// `application/json; charset=utf-8` is still accepted (parameters
    /// after the `;` are fine).
    #[test]
    fn content_type_application_json_with_charset_returns_204() {
        let req = request_with(
            Method::POST,
            "application/json; charset=utf-8",
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
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }
}
