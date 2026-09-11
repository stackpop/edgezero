// Compile-time check: SpinKvStore and SpinSecretStore implement their
// respective core store traits.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
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
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[cfg_attr(
    feature = "test-utils",
    expect(
        clippy::arbitrary_source_item_ordering,
        reason = "ingress contracts are grouped after the provider fixture tests"
    )
)]
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
    #[cfg(feature = "test-utils")]
    use edgezero_adapter_spin::request::deadline_body_releases_source_for_test;
    use edgezero_core::app::App;
    use edgezero_core::body::Body;
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{Response, StatusCode, request_builder, response_builder};
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
    #[expect(
        clippy::missing_trait_methods,
        reason = "the test provider intentionally exercises the bounded-read compatibility default"
    )]
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
    #[expect(
        clippy::missing_trait_methods,
        reason = "the test provider intentionally exercises the bounded-read compatibility default"
    )]
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
            let body = Body::text(ctx.uri().to_string());
            let response = response_builder()
                .status(StatusCode::OK)
                .body(body)
                .expect("response");
            Ok(response)
        }

        async fn mirror_body(ctx: RequestContext) -> Result<Response, EdgeError> {
            let bytes = ctx.body_bytes(1024 * 1024).await?.to_vec();
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

    #[cfg(feature = "test-utils")]
    #[test]
    fn deadline_body_releases_source_when_timeout_is_emitted() {
        assert!(deadline_body_releases_source_for_test());
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

    #[cfg(feature = "test-utils")]
    mod ingress_contract {
        use std::io;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use std::task::Poll;

        use edgezero_adapter_spin::request::dispatch_ingress_stream_for_test;
        use edgezero_core::http::{HeaderMap, HeaderValue, Method};
        use edgezero_core::ingress::{AdmissionDecision, BufferedIngressResponse, IngressGrant};
        use edgezero_core::middleware::{Middleware, Next};
        use edgezero_core::router::RouteResolution;
        use edgezero_core::time::{MonotonicClock, MonotonicInstant};
        use futures::FutureExt as _;
        use futures::stream::poll_fn;
        use http_body_util::BodyExt as _;

        use super::*;

        struct DropSignal(Arc<AtomicUsize>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        struct CountingMiddleware(Arc<AtomicUsize>);

        #[async_trait::async_trait(?Send)]
        impl Middleware for CountingMiddleware {
            async fn handle(
                &self,
                ctx: RequestContext,
                next: Next<'_>,
            ) -> Result<Response, EdgeError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                next.run(ctx).await
            }
        }

        fn guarded_fallback_app() -> (App, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let handler_calls = Arc::new(AtomicUsize::new(0));
            let handler_counter = Arc::clone(&handler_calls);
            let middleware_calls = Arc::new(AtomicUsize::new(0));
            let router = RouterService::builder()
                .get("/known", move |_ctx: RequestContext| {
                    let request_handler_calls = Arc::clone(&handler_counter);
                    async move {
                        request_handler_calls.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, EdgeError>("handler must not run")
                    }
                })
                .middleware(CountingMiddleware(Arc::clone(&middleware_calls)))
                .build();
            (App::new(router), handler_calls, middleware_calls)
        }

        fn fallback_app(
            max_body_bytes: usize,
            read_budget: Duration,
            grant_drops: &Arc<AtomicUsize>,
        ) -> (App, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let (mut app, handler_calls, middleware_calls) = guarded_fallback_app();
            let observed_grant_drops = Arc::clone(grant_drops);
            app.set_ingress_admission_policy(move |head| {
                assert!(matches!(
                    head.route_resolution(),
                    RouteResolution::MethodNotAllowed { .. } | RouteResolution::NotFound
                ));
                AdmissionDecision::ReadBodyBeforeFallback {
                    grant: IngressGrant::new(DropSignal(Arc::clone(&observed_grant_drops))),
                    max_body_bytes,
                    read_deadline: head.read_deadline_after(read_budget),
                    on_exceeded: terminal_response(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "overflow",
                        b"spin overflow\0response",
                    ),
                    on_timeout: terminal_response(
                        StatusCode::GATEWAY_TIMEOUT,
                        "timeout",
                        b"spin timeout response\n",
                    ),
                }
            });
            (app, handler_calls, middleware_calls)
        }

        fn terminal_headers(marker: &'static str) -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert(
                "content-type",
                HeaderValue::from_static("application/octet-stream"),
            );
            headers.insert("x-ingress-terminal", HeaderValue::from_static(marker));
            headers
        }

        fn terminal_response(
            status: StatusCode,
            marker: &'static str,
            body: &'static [u8],
        ) -> BufferedIngressResponse {
            BufferedIngressResponse::new(status, terminal_headers(marker), Bytes::from_static(body))
        }

        fn internal_error_headers() -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert("content-length", HeaderValue::from_static("76"));
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            headers
        }

        fn tracked_stream(
            chunks: Vec<Bytes>,
            grant_drops: &Arc<AtomicUsize>,
            source_drops: &Arc<AtomicUsize>,
            body_polls: &Arc<AtomicUsize>,
            expire_on_poll: Option<(Arc<Mutex<MonotonicInstant>>, MonotonicInstant)>,
        ) -> impl futures::Stream<Item = Result<Bytes, io::Error>> + 'static {
            let observed_grant_drops = Arc::clone(grant_drops);
            let observed_body_polls = Arc::clone(body_polls);
            let source_drop = DropSignal(Arc::clone(source_drops));
            let mut pending_chunks = chunks.into_iter();
            poll_fn(move |_cx| {
                let _keep_source_alive = &source_drop;
                assert_eq!(observed_grant_drops.load(Ordering::SeqCst), 0);
                observed_body_polls.fetch_add(1, Ordering::SeqCst);
                if let Some((now, deadline)) = &expire_on_poll {
                    *now.lock().expect("clock lock") = *deadline;
                }
                Poll::Ready(pending_chunks.next().map(Ok))
            })
        }

        fn error_stream(
            grant_drops: &Arc<AtomicUsize>,
            source_drops: &Arc<AtomicUsize>,
            body_polls: &Arc<AtomicUsize>,
        ) -> impl futures::Stream<Item = Result<Bytes, io::Error>> + 'static {
            let observed_grant_drops = Arc::clone(grant_drops);
            let observed_body_polls = Arc::clone(body_polls);
            let source_drop = DropSignal(Arc::clone(source_drops));
            let mut emitted = false;
            poll_fn(move |_cx| {
                let _keep_source_alive = &source_drop;
                assert_eq!(observed_grant_drops.load(Ordering::SeqCst), 0);
                observed_body_polls.fetch_add(1, Ordering::SeqCst);
                if emitted {
                    Poll::Ready(None)
                } else {
                    emitted = true;
                    Poll::Ready(Some(Err(io::Error::other("spin ingress source failure"))))
                }
            })
        }

        fn pending_stream(
            grant_drops: &Arc<AtomicUsize>,
            source_drops: &Arc<AtomicUsize>,
            body_polls: &Arc<AtomicUsize>,
        ) -> impl futures::Stream<Item = Result<Bytes, io::Error>> + 'static {
            let observed_grant_drops = Arc::clone(grant_drops);
            let observed_body_polls = Arc::clone(body_polls);
            let source_drop = DropSignal(Arc::clone(source_drops));
            poll_fn(move |_cx| {
                let _keep_source_alive = &source_drop;
                assert_eq!(observed_grant_drops.load(Ordering::SeqCst), 0);
                observed_body_polls.fetch_add(1, Ordering::SeqCst);
                Poll::Pending
            })
        }

        fn assert_no_route_dispatch(handler_calls: &AtomicUsize, middleware_calls: &AtomicUsize) {
            assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
            assert_eq!(middleware_calls.load(Ordering::SeqCst), 0);
        }

        fn assert_terminal_response(
            response: edgezero_adapter_spin::SpinFullResponse,
            status: StatusCode,
            expected_headers: &HeaderMap,
            body: &[u8],
        ) {
            assert_eq!(response.status(), status);
            assert_eq!(response.headers(), expected_headers);
            let bytes = block_on(response.into_body().collect())
                .expect("provider body")
                .to_bytes();
            assert_eq!(bytes.as_ref(), body);
        }

        #[test]
        fn exact_cap_preserves_not_found_and_method_not_allowed() {
            for (path, expected_status) in [
                ("/missing", StatusCode::NOT_FOUND),
                ("/known", StatusCode::METHOD_NOT_ALLOWED),
            ] {
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_polls = Arc::new(AtomicUsize::new(0));
                let (app, handler_calls, middleware_calls) =
                    fallback_app(4, Duration::from_secs(30), &grant_drops);
                let source = tracked_stream(
                    vec![Bytes::from_static(b"ab"), Bytes::from_static(b"cd")],
                    &grant_drops,
                    &source_drops,
                    &body_polls,
                    None,
                );

                let response = block_on(dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                ))
                .expect("response");

                assert_eq!(response.status(), expected_status);
                assert_eq!(body_polls.load(Ordering::SeqCst), 3);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[test]
        fn cap_plus_one_precedes_not_found_and_method_not_allowed() {
            for path in ["/missing", "/known"] {
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_polls = Arc::new(AtomicUsize::new(0));
                let (app, handler_calls, middleware_calls) =
                    fallback_app(4, Duration::from_secs(30), &grant_drops);
                let source = tracked_stream(
                    vec![Bytes::from_static(b"abcd"), Bytes::from_static(b"e")],
                    &grant_drops,
                    &source_drops,
                    &body_polls,
                    None,
                );

                let response = block_on(dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                ))
                .expect("response");

                assert_terminal_response(
                    response,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &terminal_headers("overflow"),
                    b"spin overflow\0response",
                );
                assert_eq!(body_polls.load(Ordering::SeqCst), 2);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[test]
        fn best_effort_pre_read_expiry_preserves_application_response() {
            for path in ["/missing", "/known"] {
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_polls = Arc::new(AtomicUsize::new(0));
                let start = MonotonicInstant::now();
                let (mut app, handler_calls, middleware_calls) =
                    fallback_app(4, Duration::ZERO, &grant_drops);
                app.set_monotonic_clock(MonotonicClock::new(move || start));
                let source = tracked_stream(
                    vec![Bytes::from_static(b"body")],
                    &grant_drops,
                    &source_drops,
                    &body_polls,
                    None,
                );

                let response = block_on(dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                ))
                .expect("response");

                assert_terminal_response(
                    response,
                    StatusCode::GATEWAY_TIMEOUT,
                    &terminal_headers("timeout"),
                    b"spin timeout response\n",
                );
                assert_eq!(body_polls.load(Ordering::SeqCst), 0);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[test]
        fn best_effort_post_ready_expiry_preserves_application_response() {
            for path in ["/missing", "/known"] {
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_polls = Arc::new(AtomicUsize::new(0));
                let start = MonotonicInstant::now();
                let deadline = start.checked_add(Duration::from_secs(1)).expect("deadline");
                let now = Arc::new(Mutex::new(start));
                let observed_now = Arc::clone(&now);
                let (mut app, handler_calls, middleware_calls) =
                    fallback_app(4, Duration::from_secs(1), &grant_drops);
                app.set_monotonic_clock(MonotonicClock::new(move || {
                    *observed_now.lock().expect("clock lock")
                }));
                let source = tracked_stream(
                    vec![Bytes::from_static(b"body")],
                    &grant_drops,
                    &source_drops,
                    &body_polls,
                    Some((now, deadline)),
                );

                let response = block_on(dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                ))
                .expect("response");

                assert_terminal_response(
                    response,
                    StatusCode::GATEWAY_TIMEOUT,
                    &terminal_headers("timeout"),
                    b"spin timeout response\n",
                );
                assert_eq!(body_polls.load(Ordering::SeqCst), 1);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[test]
        fn saturated_fallback_refuses_without_polling_body() {
            for path in ["/missing", "/known"] {
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_polls = Arc::new(AtomicUsize::new(0));
                let (mut app, handler_calls, middleware_calls) = guarded_fallback_app();
                app.set_ingress_admission_policy(|head| {
                    assert!(matches!(
                        head.route_resolution(),
                        RouteResolution::MethodNotAllowed { .. } | RouteResolution::NotFound
                    ));
                    AdmissionDecision::Refuse(
                        response_builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .header("x-ingress-refusal", "saturated")
                            .body(Body::from("spin unavailable\n"))
                            .expect("refusal response"),
                    )
                });
                let unobserved_grant = Arc::new(AtomicUsize::new(0));
                let source = tracked_stream(
                    vec![Bytes::from_static(b"body")],
                    &unobserved_grant,
                    &source_drops,
                    &body_polls,
                    None,
                );

                let response = block_on(dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                ))
                .expect("response");

                let mut expected_headers = HeaderMap::new();
                expected_headers.insert("x-ingress-refusal", HeaderValue::from_static("saturated"));
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(response.headers(), &expected_headers);
                let bytes = block_on(response.into_body().collect())
                    .expect("provider body")
                    .to_bytes();
                assert_eq!(bytes.as_ref(), b"spin unavailable\n");
                assert_eq!(body_polls.load(Ordering::SeqCst), 0);
                assert_eq!(unobserved_grant.load(Ordering::SeqCst), 0);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[test]
        fn source_error_uses_spin_response_boundary_and_releases_lifecycle() {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let source_drops = Arc::new(AtomicUsize::new(0));
            let body_polls = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                fallback_app(4, Duration::from_secs(30), &grant_drops);
            let source = error_stream(&grant_drops, &source_drops, &body_polls);

            let response = block_on(dispatch_ingress_stream_for_test(
                &app,
                Method::POST,
                "/missing".parse().expect("URI"),
                source,
            ))
            .expect("standard error response");

            assert_terminal_response(
                response,
                StatusCode::INTERNAL_SERVER_ERROR,
                &internal_error_headers(),
                br#"{"error":{"kind":"internal","message":"internal server error","status":500}}"#,
            );
            assert_eq!(body_polls.load(Ordering::SeqCst), 1);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
        }

        #[test]
        fn poll_then_drop_releases_pending_source_and_grant() {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let source_drops = Arc::new(AtomicUsize::new(0));
            let body_polls = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                fallback_app(4, Duration::from_secs(30), &grant_drops);
            let source = pending_stream(&grant_drops, &source_drops, &body_polls);

            let outcome = dispatch_ingress_stream_for_test(
                &app,
                Method::POST,
                "/missing".parse().expect("URI"),
                source,
            )
            .now_or_never();

            assert!(outcome.is_none());
            assert_eq!(body_polls.load(Ordering::SeqCst), 1);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
        }
    }
}

#[cfg(test)]
#[cfg(all(feature = "test-utils", not(target_arch = "wasm32")))]
mod outbound_contract_tests {
    use bytes::Bytes;
    use edgezero_adapter_spin::outbound::validate_batch_request_for_test;
    use edgezero_core::body::Body;
    use edgezero_core::http::{Method, Uri};
    use edgezero_core::outbound::OutboundRequest;
    use futures_util::stream;

    fn request() -> OutboundRequest {
        OutboundRequest::new(
            Method::POST,
            "https://example.com/bid".parse::<Uri>().expect("URI"),
        )
        .expect("request")
    }

    #[test]
    fn send_all_preflight_precedence_and_indices() {
        let requests = [
            request().body(Bytes::from_static(b"buffered")),
            request().body(Body::stream(stream::once(async {
                Bytes::from_static(b"streamed")
            }))),
            request().stream_response(),
            request().body(Bytes::from_static(b"last")),
        ];

        let outcomes: Vec<_> = requests
            .iter()
            .map(validate_batch_request_for_test)
            .collect();

        outcomes[0].as_ref().expect("buffered slot 0");
        assert!(outcomes[1].is_err(), "streamed upload keeps slot index 1");
        assert!(outcomes[2].is_err(), "streamed response keeps slot index 2");
        outcomes[3].as_ref().expect("buffered slot 3");
    }
}
