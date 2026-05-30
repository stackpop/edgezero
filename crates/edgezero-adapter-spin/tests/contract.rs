// Adapter contract tests run on the Spin wasm32 target, matching the
// fastly and cloudflare contract suites. Gating the whole file keeps the
// host `cargo test`/`clippy` runs consistent across adapters.
#![cfg(all(feature = "spin", target_arch = "wasm32"))]

use bytes::Bytes;
use edgezero_adapter_spin::context::SpinRequestContext;
use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, response_builder, Response, StatusCode};
use edgezero_core::key_value_store::{KvError, KvHandle, KvPage, KvStore};
use edgezero_core::router::RouterService;
use edgezero_core::secret_store::{SecretError, SecretHandle, SecretStore};
use futures::executor::block_on;
use futures::stream;
use std::sync::Arc;

/// Config store that returns a value only for the expected key.
struct FixedConfigStore {
    key: &'static str,
    value: &'static str,
}

impl ConfigStore for FixedConfigStore {
    fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        if key == self.key {
            Ok(Some(self.value.to_owned()))
        } else {
            Ok(None)
        }
    }
}

/// KV store that returns a fixed value for one key; everything else is absent.
struct FixedKvStore {
    key: &'static str,
    value: &'static [u8],
}

#[async_trait::async_trait(?Send)]
impl KvStore for FixedKvStore {
    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        if key == self.key {
            Ok(Some(Bytes::from_static(self.value)))
        } else {
            Ok(None)
        }
    }
    async fn put_bytes(&self, _key: &str, _value: Bytes) -> Result<(), KvError> {
        Ok(())
    }
    async fn put_bytes_with_ttl(
        &self,
        _key: &str,
        _value: Bytes,
        _ttl: std::time::Duration,
    ) -> Result<(), KvError> {
        Ok(())
    }
    async fn delete(&self, _key: &str) -> Result<(), KvError> {
        Ok(())
    }
    async fn exists(&self, key: &str) -> Result<bool, KvError> {
        Ok(key == self.key)
    }
    async fn list_keys_page(
        &self,
        _prefix: &str,
        _cursor: Option<&str>,
        _limit: usize,
    ) -> Result<KvPage, KvError> {
        Ok(KvPage {
            keys: vec![self.key.to_owned()],
            cursor: None,
        })
    }
}

/// Secret store that returns a fixed value for one (store, key) pair.
struct FixedSecretStore {
    key: &'static str,
    value: &'static [u8],
}

#[async_trait::async_trait(?Send)]
impl SecretStore for FixedSecretStore {
    async fn get_bytes(&self, _store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError> {
        if key == self.key {
            Ok(Some(Bytes::from_static(self.value)))
        } else {
            Ok(None)
        }
    }
}

fn build_test_app() -> App {
    async fn capture_uri(ctx: RequestContext) -> Result<Response, EdgeError> {
        let body = Body::text(ctx.request().uri().to_string());
        let response = response_builder()
            .status(StatusCode::OK)
            .body(body)
            .expect("response");
        Ok(response)
    }

    async fn mirror_body(ctx: RequestContext) -> Result<Response, EdgeError> {
        let bytes = ctx
            .request()
            .body()
            .as_bytes()
            .expect("buffered request body")
            .to_vec();
        let response = response_builder()
            .status(StatusCode::OK)
            .body(Body::from(bytes))
            .expect("response");
        Ok(response)
    }

    async fn stream_response(_ctx: RequestContext) -> Result<Response, EdgeError> {
        let chunks = stream::iter(vec![
            Bytes::from_static(b"chunk-1"),
            Bytes::from_static(b"chunk-2"),
        ]);

        let response = response_builder()
            .status(StatusCode::OK)
            .body(Body::stream(chunks))
            .expect("response");
        Ok(response)
    }

    async fn config_value(ctx: RequestContext) -> Result<Response, EdgeError> {
        let value = ctx
            .config_store()
            .and_then(|store| store.get("greeting").ok().flatten())
            .unwrap_or_else(|| "missing".to_owned());
        let response = response_builder()
            .status(StatusCode::OK)
            .body(Body::text(value))
            .expect("response");
        Ok(response)
    }

    async fn kv_value(ctx: RequestContext) -> Result<Response, EdgeError> {
        let value = if let Some(handle) = ctx.kv_handle() {
            match handle.get_bytes("test-key").await {
                Ok(Some(b)) => String::from_utf8_lossy(&b).into_owned(),
                Ok(None) => "missing".to_owned(),
                Err(_) => "error".to_owned(),
            }
        } else {
            "no-handle".to_owned()
        };
        let response = response_builder()
            .status(StatusCode::OK)
            .body(Body::text(value))
            .expect("response");
        Ok(response)
    }

    async fn secret_value(ctx: RequestContext) -> Result<Response, EdgeError> {
        let value = if let Some(handle) = ctx.secret_handle() {
            match handle.get_bytes("default", "test-secret").await {
                Ok(Some(b)) => String::from_utf8_lossy(&b).into_owned(),
                Ok(None) => "missing".to_owned(),
                Err(_) => "error".to_owned(),
            }
        } else {
            "no-handle".to_owned()
        };
        let response = response_builder()
            .status(StatusCode::OK)
            .body(Body::text(value))
            .expect("response");
        Ok(response)
    }

    let router = RouterService::builder()
        .get("/uri", capture_uri)
        .post("/mirror", mirror_body)
        .get("/stream", stream_response)
        .get("/config", config_value)
        .get("/kv-value", kv_value)
        .get("/secret-value", secret_value)
        .build();

    App::new(router)
}

// ---------------------------------------------------------------------------
// Tests that run on the host (no WASI runtime required)
// ---------------------------------------------------------------------------

#[test]
fn context_default_is_empty() {
    let ctx = SpinRequestContext {
        client_addr: None,
        full_url: None,
    };
    assert!(ctx.client_addr.is_none());
    assert!(ctx.full_url.is_none());
}

#[test]
fn build_test_app_creates_valid_router() {
    // Smoke test: ensure the router builds without panicking and that
    // the test helpers are usable for future integration tests.
    let _app = build_test_app();
}

#[test]
fn router_dispatches_get_and_returns_response() {
    let app = build_test_app();
    let request = request_builder()
        .method("GET")
        .uri("http://example.com/uri")
        .body(Body::empty())
        .expect("request");

    let response = block_on(app.router().oneshot(request)).expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.body().as_bytes().expect("buffered body"),
        b"http://example.com/uri"
    );
}

#[test]
fn router_dispatches_post_with_body() {
    let app = build_test_app();
    let request = request_builder()
        .method("POST")
        .uri("http://example.com/mirror")
        .body(Body::from(b"echo-payload".to_vec()))
        .expect("request");

    let response = block_on(app.router().oneshot(request)).expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.body().as_bytes().expect("buffered body"),
        b"echo-payload"
    );
}

#[test]
fn router_dispatches_streaming_route() {
    let app = build_test_app();
    let request = request_builder()
        .method("GET")
        .uri("http://example.com/stream")
        .body(Body::empty())
        .expect("request");

    let response = block_on(app.router().oneshot(request)).expect("response");

    assert_eq!(response.status(), StatusCode::OK);

    let (_, body) = response.into_parts();
    let mut stream = body.into_stream().expect("should be a stream");
    let collected = block_on(async {
        use futures::StreamExt as _;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        out
    });
    assert_eq!(collected, b"chunk-1chunk-2");
}

// ---------------------------------------------------------------------------
// Store injection smoke tests (host-side, no Spin runtime required)
// ---------------------------------------------------------------------------

#[test]
fn config_store_reads_value_from_handler() {
    let app = build_test_app();
    let mut request = request_builder()
        .method("GET")
        .uri("http://example.com/config")
        .body(Body::empty())
        .expect("request");
    request
        .extensions_mut()
        .insert(ConfigStoreHandle::new(Arc::new(FixedConfigStore {
            key: "greeting",
            value: "hello-spin",
        })));

    let response = block_on(app.router().oneshot(request)).expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.body().as_bytes().expect("buffered body"),
        b"hello-spin"
    );
}

#[test]
fn kv_store_reads_value_from_handler() {
    let app = build_test_app();
    let mut request = request_builder()
        .method("GET")
        .uri("http://example.com/kv-value")
        .body(Body::empty())
        .expect("request");
    request
        .extensions_mut()
        .insert(KvHandle::new(Arc::new(FixedKvStore {
            key: "test-key",
            value: b"kv-payload",
        })));

    let response = block_on(app.router().oneshot(request)).expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.body().as_bytes().expect("buffered body"),
        b"kv-payload"
    );
}

#[test]
fn secret_store_reads_value_from_handler() {
    let app = build_test_app();
    let mut request = request_builder()
        .method("GET")
        .uri("http://example.com/secret-value")
        .body(Body::empty())
        .expect("request");
    request
        .extensions_mut()
        .insert(SecretHandle::new(Arc::new(FixedSecretStore {
            key: "test-secret",
            value: b"s3cr3t",
        })));

    let response = block_on(app.router().oneshot(request)).expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.body().as_bytes().expect("buffered body"),
        b"s3cr3t"
    );
}

#[test]
fn missing_store_handles_return_absent_values_in_handler() {
    let app = build_test_app();

    let config_req = request_builder()
        .method("GET")
        .uri("http://example.com/config")
        .body(Body::empty())
        .expect("request");
    assert_eq!(
        block_on(app.router().oneshot(config_req))
            .expect("response")
            .body()
            .as_bytes()
            .expect("buffered body"),
        b"missing"
    );

    let kv_req = request_builder()
        .method("GET")
        .uri("http://example.com/kv-value")
        .body(Body::empty())
        .expect("request");
    assert_eq!(
        block_on(app.router().oneshot(kv_req))
            .expect("response")
            .body()
            .as_bytes()
            .expect("buffered body"),
        b"no-handle"
    );

    let secret_req = request_builder()
        .method("GET")
        .uri("http://example.com/secret-value")
        .body(Body::empty())
        .expect("request");
    assert_eq!(
        block_on(app.router().oneshot(secret_req))
            .expect("response")
            .body()
            .as_bytes()
            .expect("buffered body"),
        b"no-handle"
    );
}

// ---------------------------------------------------------------------------
// Tests that require `spin_sdk` types (wasm32 + spin feature only)
//
// `from_core_response` returns `spin_sdk::http::Response` which is only
// available on wasm32.  `into_core_request` and `dispatch` additionally
// require a WASI `IncomingRequest` handle from the Spin runtime.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod wasm {
    use super::*;
    use edgezero_adapter_spin::response::from_core_response;

    #[test]
    fn from_core_response_translates_status_and_headers() {
        futures::executor::block_on(async {
            let response = response_builder()
                .status(StatusCode::CREATED)
                .header("x-edgezero-res", "1")
                .body(Body::from(b"hello".to_vec()))
                .expect("response");

            let spin_response = from_core_response(response).await.expect("spin response");

            assert_eq!(*spin_response.status(), 201);
            let header = spin_response
                .headers()
                .find(|(name, _)| *name == "x-edgezero-res");
            assert!(header.is_some());
        });
    }

    #[test]
    fn from_core_response_collects_streaming_body() {
        futures::executor::block_on(async {
            let response = response_builder()
                .status(StatusCode::OK)
                .body(Body::stream(stream::iter(vec![
                    Bytes::from_static(b"chunk-1"),
                    Bytes::from_static(b"chunk-2"),
                ])))
                .expect("response");

            let spin_response = from_core_response(response).await.expect("spin response");

            assert_eq!(*spin_response.status(), 200);
            assert_eq!(spin_response.into_body(), b"chunk-1chunk-2");
        });
    }

    #[test]
    fn from_core_response_handles_empty_body() {
        futures::executor::block_on(async {
            let response = response_builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::from(Vec::new()))
                .expect("response");

            let spin_response = from_core_response(response).await.expect("spin response");

            assert_eq!(*spin_response.status(), 204);
            assert!(spin_response.into_body().is_empty());
        });
    }
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod store_trait_compile_checks {
    use edgezero_adapter_spin::key_value_store::SpinKvStore;
    use edgezero_adapter_spin::secret_store::SpinSecretStore;
    use edgezero_core::key_value_store::KvStore;
    use edgezero_core::secret_store::SecretStore;

    fn _assert_kv_impl<T: KvStore>() {}
    fn _assert_secret_impl<T: SecretStore>() {}
    fn _check() {
        _assert_kv_impl::<SpinKvStore>();
        _assert_secret_impl::<SpinSecretStore>();
    }
}
