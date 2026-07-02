// Adapter contract tests run on the Spin wasm32 target, matching the
// fastly and cloudflare contract suites. Gating the whole file keeps the
// host `cargo test`/`clippy` runs consistent across adapters.
#![cfg(all(feature = "spin", target_arch = "wasm32"))]

// Compile-time check: SpinKvStore and SpinSecretStore implement their
// respective core store traits.
mod store_trait_compile_checks {
    use edgezero_adapter_spin::key_value_store::SpinKvStore;
    use edgezero_adapter_spin::secret_store::SpinSecretStore;
    use edgezero_core::key_value_store::KvStore;
    use edgezero_core::secret_store::SecretStore;

    fn assert_kv_impl<T: KvStore>() {}
    fn assert_secret_impl<T: SecretStore>() {}

    // Anonymous consts whose initializers are never called; the type bounds
    // are checked at type-check time.
    const _: fn() = assert_kv_impl::<SpinKvStore>;
    const _: fn() = assert_secret_impl::<SpinSecretStore>;
}

#[cfg(test)]
mod tests {
    // `from_core_response` tests live in a nested module so they're grouped
    // together; the `tests_outside_test_module` lint is satisfied by the
    // outer `#[cfg(test)] mod tests` wrapper.
    mod from_core_response_tests {
        use super::*;
        use edgezero_adapter_spin::response::from_core_response;
        use http_body_util::BodyExt as _;

        #[test]
        fn from_core_response_translates_status_and_headers() {
            block_on(async {
                let response = response_builder()
                    .status(StatusCode::CREATED)
                    .header("x-edgezero-res", "1")
                    .body(Body::from(b"hello".to_vec()))
                    .expect("response");

                let spin_response = from_core_response(response).await.expect("spin response");

                assert_eq!(
                    spin_response.status(),
                    StatusCode::CREATED,
                    "status translated"
                );
                assert!(
                    spin_response.headers().get("x-edgezero-res").is_some(),
                    "response header preserved"
                );
            });
        }

        #[test]
        fn from_core_response_collects_streaming_body() {
            block_on(async {
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::stream(stream::iter(vec![
                        Bytes::from_static(b"chunk-1"),
                        Bytes::from_static(b"chunk-2"),
                    ])))
                    .expect("response");

                let spin_response = from_core_response(response).await.expect("spin response");

                assert_eq!(spin_response.status(), StatusCode::OK, "status translated");
                let body = spin_response
                    .into_body()
                    .collect()
                    .await
                    .expect("collect")
                    .to_bytes();
                assert_eq!(body.as_ref(), b"chunk-1chunk-2", "streaming body collected");
            });
        }

        #[test]
        fn from_core_response_handles_empty_body() {
            block_on(async {
                let response = response_builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(Body::from(Vec::new()))
                    .expect("response");

                let spin_response = from_core_response(response).await.expect("spin response");

                assert_eq!(
                    spin_response.status(),
                    StatusCode::NO_CONTENT,
                    "status translated"
                );
                let body = spin_response
                    .into_body()
                    .collect()
                    .await
                    .expect("collect")
                    .to_bytes();
                assert!(body.is_empty(), "empty body preserved");
            });
        }
    }

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
    use edgezero_core::store_registry::{
        BoundSecretStore, ConfigRegistry, ConfigStoreBinding, KvRegistry, SecretRegistry,
    };
    use futures::executor::block_on;
    use futures::stream;
    use std::sync::Arc;
    use std::time::Duration;

    /// Config store that returns a value only for the expected key.
    struct FixedConfigStore {
        key: &'static str,
        value: &'static str,
    }

    #[async_trait::async_trait(?Send)]
    impl ConfigStore for FixedConfigStore {
        async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
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
        async fn delete(&self, _key: &str) -> Result<(), KvError> {
            Ok(())
        }
        async fn exists(&self, key: &str) -> Result<bool, KvError> {
            Ok(key == self.key)
        }
        async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
            if key == self.key {
                Ok(Some(Bytes::from_static(self.value)))
            } else {
                Ok(None)
            }
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
        async fn put_bytes(&self, _key: &str, _value: Bytes) -> Result<(), KvError> {
            Ok(())
        }
        async fn put_bytes_with_ttl(
            &self,
            _key: &str,
            _value: Bytes,
            _ttl: Duration,
        ) -> Result<(), KvError> {
            Ok(())
        }
    }

    /// Secret store that returns a fixed value for one (store, key) pair.
    struct FixedSecretStore {
        key: &'static str,
        value: &'static [u8],
    }

    #[async_trait::async_trait(?Send)]
    impl SecretStore for FixedSecretStore {
        async fn get_bytes(
            &self,
            _store_name: &str,
            key: &str,
        ) -> Result<Option<Bytes>, SecretError> {
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
            // Hard-cutoff: legacy `ctx.config_handle()` is
            // gone. The dispatch boundary synthesises a one-id
            // `ConfigRegistry` from the wired handle.
            let value = match ctx.config_store_default() {
                Some(store) => store
                    .get("greeting")
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "missing".to_owned()),
                None => "missing".to_owned(),
            };
            let response = response_builder()
                .status(StatusCode::OK)
                .body(Body::text(value))
                .expect("response");
            Ok(response)
        }

        async fn kv_value(ctx: RequestContext) -> Result<Response, EdgeError> {
            // Hard-cutoff: `ctx.kv_handle()` removed —
            // `kv_store_default()` returns a `BoundKvStore` (alias
            // for `KvHandle`) with the same `get_bytes` method.
            let value = if let Some(handle) = ctx.kv_store_default() {
                match handle.get_bytes("test-key").await {
                    Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
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
            // Hard-cutoff: `ctx.secret_handle()` removed.
            // `secret_store_default()` returns a `BoundSecretStore`,
            // which bundles the platform store name with the handle —
            // so the lookup is `bound.get_bytes(key)` (single arg),
            // not `handle.get_bytes(store_name, key)` (two args).
            let value = if let Some(bound) = ctx.secret_store_default() {
                match bound.get_bytes("test-secret").await {
                    Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
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

    #[test]
    fn context_default_is_empty() {
        let ctx = SpinRequestContext {
            client_addr: None,
            full_url: None,
        };
        assert!(ctx.client_addr.is_none(), "client_addr defaults to None");
        assert!(ctx.full_url.is_none(), "full_url defaults to None");
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

        assert_eq!(response.status(), StatusCode::OK, "status OK");
        assert_eq!(
            response.body().as_bytes().expect("buffered body"),
            b"http://example.com/uri",
            "uri echoed"
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

        assert_eq!(response.status(), StatusCode::OK, "status OK");
        assert_eq!(
            response.body().as_bytes().expect("buffered body"),
            b"echo-payload",
            "body echoed"
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

        assert_eq!(response.status(), StatusCode::OK, "status OK");

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
        assert_eq!(collected, b"chunk-1chunk-2", "chunks concatenated");
    }

    #[test]
    fn config_store_reads_value_from_handler() {
        let app = build_test_app();
        let mut request = request_builder()
            .method("GET")
            .uri("http://example.com/config")
            .body(Body::empty())
            .expect("request");
        // Mirror the dispatch boundary: the runtime synthesises a one-id
        // `ConfigRegistry` keyed under `"default"` from the wired handle.
        // `RequestContext::config_store_default()` reads `ConfigRegistry`
        // only (hard-cutoff), so inserting a bare handle here would yield
        // `None` and the handler would return "missing".
        let handle = ConfigStoreHandle::new(Arc::new(FixedConfigStore {
            key: "greeting",
            value: "hello-spin",
        }));
        request.extensions_mut().insert(ConfigRegistry::single_id(
            "default".to_owned(),
            ConfigStoreBinding {
                handle,
                default_key: "default".to_owned(),
            },
        ));

        let response = block_on(app.router().oneshot(request)).expect("response");

        assert_eq!(response.status(), StatusCode::OK, "status OK");
        assert_eq!(
            response.body().as_bytes().expect("buffered body"),
            b"hello-spin",
            "config value passed through"
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
        let handle = KvHandle::new(Arc::new(FixedKvStore {
            key: "test-key",
            value: b"kv-payload",
        }));
        request
            .extensions_mut()
            .insert(KvRegistry::single_id("default".to_owned(), handle));

        let response = block_on(app.router().oneshot(request)).expect("response");

        assert_eq!(response.status(), StatusCode::OK, "status OK");
        assert_eq!(
            response.body().as_bytes().expect("buffered body"),
            b"kv-payload",
            "kv value passed through"
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
        // Secrets registry wraps the handle in a `BoundSecretStore` carrying
        // the platform store name — mirrors the dispatch-boundary synthesis.
        let handle = SecretHandle::new(Arc::new(FixedSecretStore {
            key: "test-secret",
            value: b"s3cr3t",
        }));
        request.extensions_mut().insert(SecretRegistry::single_id(
            "default".to_owned(),
            BoundSecretStore::new(handle, "default".to_owned()),
        ));

        let response = block_on(app.router().oneshot(request)).expect("response");

        assert_eq!(response.status(), StatusCode::OK, "status OK");
        assert_eq!(
            response.body().as_bytes().expect("buffered body"),
            b"s3cr3t",
            "secret value passed through"
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
            b"missing",
            "no config store falls through to handler default"
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
            b"no-handle",
            "no kv handle yields the no-handle marker"
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
            b"no-handle",
            "no secret handle yields the no-handle marker"
        );
    }
}
