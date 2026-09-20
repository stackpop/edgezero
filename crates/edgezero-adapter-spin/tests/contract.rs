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
    use bytes::Bytes;
    #[cfg(feature = "test-utils")]
    use edgezero_adapter_spin::build_app_for_test;
    use edgezero_adapter_spin::context::SpinRequestContext;
    #[cfg(feature = "test-utils")]
    use edgezero_adapter_spin::request::deadline_body_releases_source_for_test;
    use edgezero_core::app::App;
    #[cfg(feature = "test-utils")]
    use edgezero_core::app::Hooks;
    use edgezero_core::body::Body;
    use edgezero_core::config_store::{
        BoundedStoreRead, ConfigStore, ConfigStoreError, ConfigStoreHandle,
        finish_bounded_config_read,
    };
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{Response, StatusCode, request_builder, response_builder};
    use edgezero_core::key_value_store::{KvError, KvHandle, KvPage, KvStore};
    use edgezero_core::router::RouterService;
    use edgezero_core::secret_store::{
        SecretError, SecretHandle, SecretStore, finish_bounded_secret_read,
    };
    use edgezero_core::store_registry::{
        BoundSecretStore, ConfigRegistry, ConfigStoreBinding, KvRegistry, SecretRegistry,
    };
    use edgezero_core::time::{Deadline, MonotonicClock};
    use futures::executor::block_on;
    use futures::stream;
    use std::sync::Arc;
    use std::time::Duration;

    /// Config store that returns a value only for the expected key.
    #[cfg(feature = "test-utils")]
    struct FailingConfiguration;

    #[cfg(feature = "test-utils")]
    #[expect(
        clippy::missing_trait_methods,
        reason = "test hook exercises only adapter startup failure"
    )]
    impl Hooks for FailingConfiguration {
        fn configure(_app: &mut App) -> Result<(), EdgeError> {
            Err(EdgeError::service_unavailable("configuration unavailable"))
        }

        fn routes() -> RouterService {
            RouterService::builder().build()
        }
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn failing_configuration() {
        let result = build_app_for_test::<FailingConfiguration>();
        assert!(result.is_err());
        let error = result.err().expect("configuration must fail");
        assert_eq!(error.to_string(), "application configuration failed");
        assert!(format!("{error:#}").contains("configuration unavailable"));
    }

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

        async fn get_bounded(
            &self,
            key: &str,
            clock: &MonotonicClock,
            deadline: Deadline,
            max_backend_bytes: u64,
            max_value_bytes: u64,
        ) -> Result<BoundedStoreRead<String>, ConfigStoreError> {
            if deadline.is_expired_at(clock.now()) {
                return Err(ConfigStoreError::DeadlineExceeded);
            }
            finish_bounded_config_read(
                self.get(key).await,
                clock,
                deadline,
                max_backend_bytes,
                max_value_bytes,
            )
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

        async fn get_bytes_bounded(
            &self,
            store_name: &str,
            key: &str,
            clock: &MonotonicClock,
            deadline: Deadline,
            max_backend_bytes: u64,
            max_value_bytes: u64,
        ) -> Result<BoundedStoreRead<Bytes>, SecretError> {
            if deadline.is_expired_at(clock.now()) {
                return Err(SecretError::DeadlineExceeded);
            }
            finish_bounded_secret_read(
                self.get_bytes(store_name, key).await,
                clock,
                deadline,
                max_backend_bytes,
                max_value_bytes,
            )
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
        use std::collections::VecDeque;
        use std::future::{Future, poll_fn as poll_future, ready};
        use std::io;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use std::task::Poll;

        use edgezero_adapter_spin::outbound::{
            SpinOutboundClient, deferred_clock_paths_hold_for_test,
        };
        use edgezero_adapter_spin::request::{
            dispatch_ingress_source_error_for_test, dispatch_ingress_stream_for_test,
            dispatch_ingress_stream_with_timer_for_test, dispatch_ingress_with_for_test,
            dispatch_request_for_test,
        };
        use edgezero_core::http::{HeaderMap, HeaderValue, Method};
        use edgezero_core::ingress::{
            AdmissionDecision, BufferedIngressResponse, IngressGrant, IngressHeadLimits,
        };
        use edgezero_core::middleware::{Middleware, Next};
        use edgezero_core::outbound::{OutboundHttpClient as _, OutboundRequest};
        use edgezero_core::response_egress::{
            DEFAULT_RESPONSE_WRITE_BUDGET, DetachedResponseEgressDecision,
            ResponseEgressCompletion, ResponseEgressEnvelope, ResponseEgressObserver,
            ResponseEgressOutcome, ResponseEgressReport,
        };
        use edgezero_core::router::{RouteMetadata, RouteResolution};
        use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
        use futures::FutureExt as _;
        use futures::stream::{empty, poll_fn};
        use spin_sdk::http::{FromRequest as _, IntoRequest as _, Request as SpinRequest};

        use super::*;

        #[test]
        fn admission_abort() {
            let source_calls = Arc::new(AtomicUsize::new(0));
            let observed_source_calls = Arc::clone(&source_calls);
            let delivery_calls = Arc::new(AtomicUsize::new(0));
            let observed_delivery_calls = Arc::clone(&delivery_calls);
            let mut app = App::new(RouterService::builder().build());
            app.set_ingress_admission_policy(|_| AdmissionDecision::Abort);

            let error = block_on(dispatch_ingress_with_for_test(
                &app,
                Method::POST,
                "/missing".parse().expect("URI"),
                move || {
                    observed_source_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(stream::empty::<Result<Bytes, io::Error>>())
                },
                move |_response| {
                    observed_delivery_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            ))
            .expect_err("abort must return a platform error");

            assert!(error.to_string().contains("ingress admission aborted"));
            assert_eq!(source_calls.load(Ordering::SeqCst), 0);
            assert_eq!(delivery_calls.load(Ordering::SeqCst), 0);
        }

        struct DropSignal(Arc<AtomicUsize>);

        #[derive(Clone)]
        struct RecordingEgressObserver(Arc<Mutex<Vec<ResponseEgressReport>>>);

        impl ResponseEgressObserver for RecordingEgressObserver {
            fn complete(&self, report: &ResponseEgressReport) {
                self.0.lock().expect("reports lock").push(report.clone());
            }
        }

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        fn scripted_clock(script: Vec<MonotonicInstant>) -> MonotonicClock {
            let observations = Arc::new(Mutex::new(VecDeque::from(script)));
            MonotonicClock::new(move || {
                observations
                    .lock()
                    .expect("clock observations")
                    .pop_front()
                    .expect("clock observation")
            })
        }

        #[test]
        fn outbound_clock_controls_preflight_elapsed_and_backwards_failure() {
            let start = MonotonicInstant::now();
            let completed = start
                .checked_add(Duration::from_millis(9))
                .expect("completed instant");
            let forward_client =
                SpinOutboundClient::with_clock(scripted_clock(vec![start, completed]));
            let forward_request = OutboundRequest::get("https://example.com/")
                .expect("request")
                .stream_response();
            let cutoff = Deadline::at_instant(
                start
                    .checked_add(Duration::from_secs(1))
                    .expect("cutoff instant"),
            );
            let forward_results = block_on(
                forward_client
                    .start_batch_until(vec![forward_request], cutoff)
                    .collect(),
            )
            .expect("valid batch driver");
            assert_eq!(
                forward_results.termination,
                edgezero_core::OutboundBatchTermination::Completed
            );
            assert_eq!(forward_results.slots.len(), 1);
            let forward_result = forward_results.slots[0]
                .as_ref()
                .expect("single forward result");
            assert_eq!(forward_result.elapsed, Duration::from_millis(9));
            assert!(matches!(
                forward_result.outcome,
                Err(EdgeError::BadRequest { .. })
            ));

            let earlier = start
                .checked_sub(Duration::from_millis(1))
                .expect("earlier instant");
            let backwards_client =
                SpinOutboundClient::with_clock(scripted_clock(vec![start, earlier]));
            let backwards_request = OutboundRequest::get("https://example.com/")
                .expect("request")
                .stream_response();
            let backwards_results = block_on(
                backwards_client
                    .start_batch_until(vec![backwards_request], cutoff)
                    .collect(),
            )
            .expect("valid batch driver");
            assert_eq!(
                backwards_results.termination,
                edgezero_core::OutboundBatchTermination::Completed
            );
            assert_eq!(backwards_results.slots.len(), 1);
            let backwards_result = backwards_results.slots[0]
                .as_ref()
                .expect("single backwards result");
            assert_eq!(backwards_result.elapsed, Duration::ZERO);
            assert!(matches!(
                backwards_result.outcome,
                Err(EdgeError::Internal { .. })
            ));
        }

        #[test]
        fn already_expired_batch_reports_cutoff() {
            let now = MonotonicInstant::now();
            let client = SpinOutboundClient::with_clock(MonotonicClock::new(move || now));
            let request = OutboundRequest::get("https://example.com/").expect("request");

            let results = block_on(
                client
                    .start_batch_until(vec![request], Deadline::at_instant(now))
                    .collect(),
            )
            .expect("valid batch driver");

            assert_eq!(
                results.termination,
                edgezero_core::OutboundBatchTermination::Cutoff
            );
            assert!(matches!(results.slots.first(), Some(None)));
        }

        #[test]
        fn batch_preserves_three_preflight_slots_in_input_order() {
            let now = MonotonicInstant::now();
            let client = SpinOutboundClient::with_clock(MonotonicClock::new(move || now));
            let streamed_upload = OutboundRequest::post("https://example.com/upload")
                .expect("upload request")
                .body(Body::stream(stream::iter([Bytes::from_static(b"body")])));
            let streamed_response = OutboundRequest::get("https://example.com/stream")
                .expect("stream request")
                .stream_response();
            let method_error = OutboundRequest::get("https://example.com/get")
                .expect("GET request")
                .body(Body::stream(stream::iter([Bytes::new()])));

            let results = block_on(
                client
                    .start_batch_until(
                        vec![streamed_upload, streamed_response, method_error],
                        Deadline::at_instant(
                            now.checked_add(Duration::from_secs(1))
                                .expect("cutoff instant"),
                        ),
                    )
                    .collect(),
            )
            .expect("valid batch driver");
            assert_eq!(
                results.termination,
                edgezero_core::OutboundBatchTermination::Completed
            );
            let messages: Vec<_> = results
                .slots
                .iter()
                .map(
                    |slot| match &slot.as_ref().expect("resolved preflight slot").outcome {
                        Err(EdgeError::BadRequest { message }) => message.as_str(),
                        other => panic!("expected preflight rejection, got {other:?}"),
                    },
                )
                .collect();

            assert_eq!(
                messages,
                [
                    "outbound batches require buffered request bodies; use send for a streamed upload",
                    "outbound batches require buffered responses; use send for a streamed response",
                    "GET/HEAD request must not carry a streamed body; emptiness cannot be determined without consuming the stream",
                ]
            );
            assert!(results.slots.iter().all(|slot| {
                slot.as_ref()
                    .is_some_and(|result| result.elapsed == Duration::ZERO)
            }));
        }

        #[test]
        fn deferred_request_and_response_paths_retain_the_injected_clock() {
            assert!(block_on(deferred_clock_paths_hold_for_test()));
        }

        #[test]
        fn standard_dispatch_installs_the_application_outbound_clock() {
            async fn elapsed(ctx: RequestContext) -> Result<String, EdgeError> {
                let client = ctx
                    .http_client()
                    .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("missing HTTP client")))?;
                let request = OutboundRequest::get("https://example.com/")?.stream_response();
                let results = client
                    .send_all_until(vec![request], Deadline::after(Duration::from_secs(1)))
                    .await?;
                let result = results
                    .slots
                    .first()
                    .and_then(Option::as_ref)
                    .ok_or_else(|| {
                        EdgeError::internal(anyhow::anyhow!("missing outbound result"))
                    })?;
                Ok(result.elapsed.as_millis().to_string())
            }

            let start = MonotonicInstant::now();
            let completed = start
                .checked_add(Duration::from_millis(7))
                .expect("completed instant");
            let observations = Arc::new(AtomicUsize::new(0));
            let clock_observations = Arc::clone(&observations);
            let mut app = App::new(RouterService::builder().get("/clock", elapsed).build());
            app.set_monotonic_clock(MonotonicClock::new(move || {
                if clock_observations.fetch_add(1, Ordering::SeqCst) < 2 {
                    start
                } else {
                    completed
                }
            }));
            let source = empty::<Result<Bytes, io::Error>>();

            let response = block_on(dispatch_ingress_stream_for_test(
                &app,
                Method::GET,
                "/clock".parse().expect("URI"),
                source,
            ))
            .expect("Spin response");

            let bytes = captured_body(response);
            assert_eq!(bytes.as_ref(), b"7");
            assert!(observations.load(Ordering::SeqCst) >= 3);
        }

        #[test]
        fn standard_native_request_refuses_before_body_read() {
            let (mut app, handler_calls, middleware_calls) = guarded_fallback_app();
            app.set_ingress_admission_policy(|head| {
                assert!(matches!(head.route_resolution(), RouteResolution::NotFound));
                AdmissionDecision::Refuse {
                    completion: ResponseEgressCompletion::empty(),
                    response: response_builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .header("x-ingress-refusal", "saturated")
                        .body(Body::from("spin unavailable\n"))
                        .expect("refusal response"),
                }
            });
            let outgoing_request = SpinRequest::builder()
                .method("POST")
                .uri("http://example.test/missing")
                .body(http_body_util::Full::new(Bytes::from_static(b"abcde")))
                .expect("outgoing-compatible request");
            let wasi_request = outgoing_request.into_request().expect("WASI request");
            let incoming_request =
                SpinRequest::from_request(wasi_request).expect("incoming Spin request");

            let response =
                block_on(dispatch_request_for_test(&app, incoming_request)).expect("Spin response");

            let mut expected_headers = HeaderMap::new();
            expected_headers.insert("x-ingress-refusal", HeaderValue::from_static("saturated"));
            assert_terminal_response(
                response,
                StatusCode::SERVICE_UNAVAILABLE,
                &expected_headers,
                b"spin unavailable\n",
            );
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
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
                    completion: ResponseEgressCompletion::empty(),
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

        fn matched_body_app(
            read_budget: Duration,
            grant_drops: &Arc<AtomicUsize>,
        ) -> (App, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let handler_calls = Arc::new(AtomicUsize::new(0));
            let handler_counter = Arc::clone(&handler_calls);
            let middleware_calls = Arc::new(AtomicUsize::new(0));
            let router = RouterService::builder()
                .post("/body", move |ctx: RequestContext| {
                    let request_handler_calls = Arc::clone(&handler_counter);
                    async move {
                        request_handler_calls.fetch_add(1, Ordering::SeqCst);
                        let _bytes = ctx.body_bytes(4).await?;
                        Ok::<_, EdgeError>("body read completed unexpectedly")
                    }
                })
                .middleware(CountingMiddleware(Arc::clone(&middleware_calls)))
                .build();
            let mut app = App::new(router);
            let observed_grant_drops = Arc::clone(grant_drops);
            app.set_ingress_admission_policy(move |head| {
                assert!(matches!(
                    head.route_resolution(),
                    RouteResolution::Matched(metadata) if metadata.pattern() == "/body"
                ));
                AdmissionDecision::Admit {
                    completion: ResponseEgressCompletion::empty(),
                    grant: IngressGrant::new(DropSignal(Arc::clone(&observed_grant_drops))),
                    read_deadline: head.read_deadline_after(read_budget),
                }
            });
            (app, handler_calls, middleware_calls)
        }

        fn deadline_timer() -> impl Future<Output = ()> {
            let mut first_poll = true;
            poll_future(move |cx| {
                if first_poll {
                    first_poll = false;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
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

        fn capture_response(envelope: ResponseEgressEnvelope) -> Response {
            let Ok((prepared, _policy, mut attempt, clock)) = envelope.begin() else {
                panic!("test egress envelope must prepare");
            };
            assert!(attempt.begin_writing());
            assert!(attempt.terminate(ResponseEgressOutcome::HostHandoff, clock.now()));
            prepared.into_response()
        }

        fn assert_terminal_response(
            envelope: ResponseEgressEnvelope,
            status: StatusCode,
            expected_headers: &HeaderMap,
            body: &[u8],
        ) {
            let response = capture_response(envelope);
            assert_eq!(response.status(), status);
            assert_eq!(response.headers(), expected_headers);
            let bytes = captured_response_body(response);
            assert_eq!(bytes.as_ref(), body);
        }

        fn captured_body(envelope: ResponseEgressEnvelope) -> Bytes {
            captured_response_body(capture_response(envelope))
        }

        #[test]
        fn normalized_ingress_error_returns_typed_response_without_application_observation() {
            let reports = Arc::new(Mutex::new(Vec::new()));
            let factory_calls = Arc::new(AtomicUsize::new(0));
            let completion_calls = Arc::new(AtomicUsize::new(0));
            let mut app = App::new(RouterService::builder().build());
            app.set_ingress_head_limits(
                IngressHeadLimits::default()
                    .with_max_request_target_bytes(4)
                    .expect("target limit"),
            );
            app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
            let observed_factory_calls = Arc::clone(&factory_calls);
            let observed_completion_calls = Arc::clone(&completion_calls);
            app.set_detached_response_egress_decision_factory(move |head| {
                observed_factory_calls.fetch_add(1, Ordering::SeqCst);
                let terminal_calls = Arc::clone(&observed_completion_calls);
                DetachedResponseEgressDecision::Send {
                    completion: ResponseEgressCompletion::new(move |_report| {
                        terminal_calls.fetch_add(1, Ordering::SeqCst);
                    }),
                    deadline: head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET),
                }
            });

            let egress = block_on(dispatch_ingress_stream_for_test(
                &app,
                Method::GET,
                "/too-long".parse().expect("URI"),
                empty::<Result<Bytes, io::Error>>(),
            ))
            .expect("typed response");
            let response = capture_response(egress);

            assert_eq!(response.status(), StatusCode::URI_TOO_LONG);
            assert!(reports.lock().expect("reports lock").is_empty());
            assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
            assert_eq!(completion_calls.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn normalized_ingress_error_decision_abort_uses_provider_error() {
            let (mut app, handler_calls, middleware_calls) = guarded_fallback_app();
            app.set_ingress_head_limits(
                IngressHeadLimits::default()
                    .with_max_request_target_bytes(4)
                    .expect("target limit"),
            );
            let reports = Arc::new(Mutex::new(Vec::new()));
            app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
            let factory_calls = Arc::new(AtomicUsize::new(0));
            let observed_factory_calls = Arc::clone(&factory_calls);
            app.set_detached_response_egress_decision_factory(move |_head| {
                observed_factory_calls.fetch_add(1, Ordering::SeqCst);
                DetachedResponseEgressDecision::Abort
            });
            let source_calls = Arc::new(AtomicUsize::new(0));
            let observed_source_calls = Arc::clone(&source_calls);
            let delivery_calls = Arc::new(AtomicUsize::new(0));
            let observed_delivery_calls = Arc::clone(&delivery_calls);

            let error = block_on(dispatch_ingress_with_for_test(
                &app,
                Method::GET,
                "/too-long".parse().expect("URI"),
                move || {
                    observed_source_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(empty::<Result<Bytes, io::Error>>())
                },
                move |_egress| {
                    observed_delivery_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            ))
            .expect_err("detached abort must return a platform error");

            assert!(error.to_string().contains("ingress admission aborted"));
            assert_eq!(source_calls.load(Ordering::SeqCst), 0);
            assert_eq!(delivery_calls.load(Ordering::SeqCst), 0);
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
            assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
            assert!(reports.lock().expect("reports lock").is_empty());
        }

        #[test]
        fn admission_policy_error_decision_send_uses_detached_completion() {
            let (mut app, handler_calls, middleware_calls) = guarded_fallback_app();
            let decision_completion_calls = Arc::new(AtomicUsize::new(0));
            let decision_resource_drops = Arc::new(AtomicUsize::new(0));
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let observed_decision_completion_calls = Arc::clone(&decision_completion_calls);
            let observed_decision_resource_drops = Arc::clone(&decision_resource_drops);
            let observed_grant_drops = Arc::clone(&grant_drops);
            app.set_ingress_admission_policy(move |head| {
                assert!(matches!(
                    head.route_resolution(),
                    RouteResolution::Matched(_)
                ));
                let decision_resource = DropSignal(Arc::clone(&observed_decision_resource_drops));
                let terminal_calls = Arc::clone(&observed_decision_completion_calls);
                AdmissionDecision::ReadBodyBeforeFallback {
                    completion: ResponseEgressCompletion::new(move |_report| {
                        let _resource = decision_resource;
                        terminal_calls.fetch_add(1, Ordering::SeqCst);
                    }),
                    grant: IngressGrant::new(DropSignal(Arc::clone(&observed_grant_drops))),
                    max_body_bytes: 1,
                    read_deadline: head.read_deadline_after(Duration::from_secs(1)),
                    on_exceeded: BufferedIngressResponse::text(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "too large",
                    ),
                    on_timeout: BufferedIngressResponse::text(
                        StatusCode::REQUEST_TIMEOUT,
                        "timeout",
                    ),
                }
            });
            let reports = Arc::new(Mutex::new(Vec::new()));
            app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
            let factory_calls = Arc::new(AtomicUsize::new(0));
            let completion_calls = Arc::new(AtomicUsize::new(0));
            let observed_factory_calls = Arc::clone(&factory_calls);
            let observed_completion_calls = Arc::clone(&completion_calls);
            app.set_detached_response_egress_decision_factory(move |head| {
                observed_factory_calls.fetch_add(1, Ordering::SeqCst);
                let terminal_calls = Arc::clone(&observed_completion_calls);
                DetachedResponseEgressDecision::Send {
                    completion: ResponseEgressCompletion::new(move |_report| {
                        terminal_calls.fetch_add(1, Ordering::SeqCst);
                    }),
                    deadline: head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET),
                }
            });
            let source_calls = Arc::new(AtomicUsize::new(0));
            let observed_source_calls = Arc::clone(&source_calls);

            let response = block_on(dispatch_ingress_with_for_test(
                &app,
                Method::GET,
                "/known".parse().expect("URI"),
                move || {
                    observed_source_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(empty::<Result<Bytes, io::Error>>())
                },
                |egress| Ok(capture_response(egress)),
            ))
            .expect("typed policy error response");

            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(source_calls.load(Ordering::SeqCst), 0);
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
            assert_eq!(decision_completion_calls.load(Ordering::SeqCst), 0);
            assert_eq!(decision_resource_drops.load(Ordering::SeqCst), 1);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
            assert_eq!(completion_calls.load(Ordering::SeqCst), 1);
            assert!(reports.lock().expect("reports lock").is_empty());
        }

        #[test]
        fn admission_policy_error_decision_abort_uses_provider_error() {
            let (mut app, handler_calls, middleware_calls) = guarded_fallback_app();
            let decision_resource_drops = Arc::new(AtomicUsize::new(0));
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let observed_decision_resource_drops = Arc::clone(&decision_resource_drops);
            let observed_grant_drops = Arc::clone(&grant_drops);
            app.set_ingress_admission_policy(move |head| {
                AdmissionDecision::ReadBodyBeforeFallback {
                    completion: ResponseEgressCompletion::new({
                        let resource = DropSignal(Arc::clone(&observed_decision_resource_drops));
                        move |_report| drop(resource)
                    }),
                    grant: IngressGrant::new(DropSignal(Arc::clone(&observed_grant_drops))),
                    max_body_bytes: 1,
                    read_deadline: head.read_deadline_after(Duration::from_secs(1)),
                    on_exceeded: BufferedIngressResponse::text(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "too large",
                    ),
                    on_timeout: BufferedIngressResponse::text(
                        StatusCode::REQUEST_TIMEOUT,
                        "timeout",
                    ),
                }
            });
            let reports = Arc::new(Mutex::new(Vec::new()));
            app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
            let factory_calls = Arc::new(AtomicUsize::new(0));
            let observed_factory_calls = Arc::clone(&factory_calls);
            app.set_detached_response_egress_decision_factory(move |_head| {
                observed_factory_calls.fetch_add(1, Ordering::SeqCst);
                DetachedResponseEgressDecision::Abort
            });
            let source_calls = Arc::new(AtomicUsize::new(0));
            let observed_source_calls = Arc::clone(&source_calls);
            let delivery_calls = Arc::new(AtomicUsize::new(0));
            let observed_delivery_calls = Arc::clone(&delivery_calls);

            let error = block_on(dispatch_ingress_with_for_test(
                &app,
                Method::GET,
                "/known".parse().expect("URI"),
                move || {
                    observed_source_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(empty::<Result<Bytes, io::Error>>())
                },
                move |_egress| {
                    observed_delivery_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            ))
            .expect_err("detached abort must return a platform error");

            assert!(error.to_string().contains("ingress admission aborted"));
            assert_eq!(source_calls.load(Ordering::SeqCst), 0);
            assert_eq!(delivery_calls.load(Ordering::SeqCst), 0);
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
            assert_eq!(decision_resource_drops.load(Ordering::SeqCst), 1);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
            assert!(reports.lock().expect("reports lock").is_empty());
        }

        #[test]
        fn post_admission_failure_uses_owned_error_egress() {
            let reports = Arc::new(Mutex::new(Vec::new()));
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let observed_grant_drops = Arc::clone(&grant_drops);
            let mut app = App::new(
                RouterService::builder()
                    .get("/owned", |_ctx: RequestContext| async move {
                        Ok::<_, EdgeError>("handler must not run")
                    })
                    .build(),
            );
            app.set_ingress_admission_policy(move |head| AdmissionDecision::Admit {
                completion: ResponseEgressCompletion::empty(),
                grant: IngressGrant::new(DropSignal(Arc::clone(&observed_grant_drops))),
                read_deadline: head.read_deadline_after(Duration::from_secs(1)),
            });
            app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));

            let egress = block_on(dispatch_ingress_source_error_for_test(
                &app,
                Method::GET,
                "/owned".parse().expect("URI"),
            ))
            .expect("owned error egress");
            let response = capture_response(egress);

            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            let observed = reports.lock().expect("reports lock");
            assert_eq!(observed.len(), 1);
            let report = observed.first().expect("one report");
            assert_eq!(report.outcome, ResponseEgressOutcome::HostHandoff);
            assert_eq!(
                report.route.as_ref().map(RouteMetadata::pattern),
                Some("/owned")
            );
        }

        fn captured_response_body(response: Response) -> Bytes {
            block_on(response.into_body().into_bytes_bounded(usize::MAX))
                .expect("captured response body")
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

                let envelope = block_on(dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                ))
                .expect("response");
                let response = capture_response(envelope);

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
        fn pending_matched_body_read_returns_408_and_releases_lifecycle() {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let source_drops = Arc::new(AtomicUsize::new(0));
            let body_polls = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                matched_body_app(Duration::from_millis(100), &grant_drops);
            let source = pending_stream(&grant_drops, &source_drops, &body_polls);

            let envelope = block_on(dispatch_ingress_stream_with_timer_for_test(
                &app,
                Method::POST,
                "/body".parse().expect("URI"),
                source,
                |_remaining| deadline_timer(),
            ))
            .expect("Spin response");
            let response = capture_response(envelope);

            assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
            assert_eq!(
                response.headers().get("content-type"),
                Some(&HeaderValue::from_static("application/json"))
            );
            let bytes = captured_response_body(response);
            assert_eq!(
                bytes.as_ref(),
                br#"{"error":{"kind":"request_timeout","message":"inbound body read deadline exceeded","status":408}}"#
            );
            assert!(body_polls.load(Ordering::SeqCst) > 0);
            assert_eq!(handler_calls.load(Ordering::SeqCst), 1);
            assert_eq!(middleware_calls.load(Ordering::SeqCst), 1);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn simultaneously_ready_timer_precedes_matched_body() {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let source_drops = Arc::new(AtomicUsize::new(0));
            let body_polls = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                matched_body_app(Duration::from_millis(100), &grant_drops);
            let source = tracked_stream(
                vec![Bytes::from_static(b"body")],
                &grant_drops,
                &source_drops,
                &body_polls,
                None,
            );

            let envelope = block_on(dispatch_ingress_stream_with_timer_for_test(
                &app,
                Method::POST,
                "/body".parse().expect("URI"),
                source,
                |_remaining| ready(()),
            ))
            .expect("Spin response");
            let response = capture_response(envelope);

            assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
            assert_eq!(body_polls.load(Ordering::SeqCst), 0);
            assert_eq!(handler_calls.load(Ordering::SeqCst), 1);
            assert_eq!(middleware_calls.load(Ordering::SeqCst), 1);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn pending_fallback_body_read_preserves_timeout_response_and_releases_lifecycle() {
            for path in ["/missing", "/known"] {
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_polls = Arc::new(AtomicUsize::new(0));
                let (app, handler_calls, middleware_calls) =
                    fallback_app(4, Duration::from_millis(100), &grant_drops);
                let source = pending_stream(&grant_drops, &source_drops, &body_polls);

                let response = block_on(dispatch_ingress_stream_with_timer_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                    |_remaining| deadline_timer(),
                ))
                .expect("Spin response");

                assert_terminal_response(
                    response,
                    StatusCode::GATEWAY_TIMEOUT,
                    &terminal_headers("timeout"),
                    b"spin timeout response\n",
                );
                assert!(body_polls.load(Ordering::SeqCst) > 0);
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
                let completion_calls = Arc::new(AtomicUsize::new(0));
                let observed_completion_calls = Arc::clone(&completion_calls);
                let (mut app, handler_calls, middleware_calls) = guarded_fallback_app();
                app.set_ingress_admission_policy(move |head| {
                    assert!(matches!(
                        head.route_resolution(),
                        RouteResolution::MethodNotAllowed { .. } | RouteResolution::NotFound
                    ));
                    AdmissionDecision::Refuse {
                        completion: {
                            let calls = Arc::clone(&observed_completion_calls);
                            ResponseEgressCompletion::new(move |_report| {
                                calls.fetch_add(1, Ordering::SeqCst);
                            })
                        },
                        response: response_builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .header("x-ingress-refusal", "saturated")
                            .body(Body::from("spin unavailable\n"))
                            .expect("refusal response"),
                    }
                });
                let unobserved_grant = Arc::new(AtomicUsize::new(0));
                let source = tracked_stream(
                    vec![Bytes::from_static(b"body")],
                    &unobserved_grant,
                    &source_drops,
                    &body_polls,
                    None,
                );

                let envelope = block_on(dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                ))
                .expect("response");
                let response = capture_response(envelope);

                let mut expected_headers = HeaderMap::new();
                expected_headers.insert("x-ingress-refusal", HeaderValue::from_static("saturated"));
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(response.headers(), &expected_headers);
                let bytes = captured_response_body(response);
                assert_eq!(bytes.as_ref(), b"spin unavailable\n");
                assert_eq!(body_polls.load(Ordering::SeqCst), 0);
                assert_eq!(unobserved_grant.load(Ordering::SeqCst), 0);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_eq!(completion_calls.load(Ordering::SeqCst), 1);
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
    use edgezero_adapter_spin::outbound::{
        validate_batch_request_for_test, validate_request_for_test,
    };
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
    fn batch_preflight_precedence_and_indices() {
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

    #[test]
    fn authority_override_fails_before_spin_dispatch() {
        let request = request()
            .host_authority_override("virtual.example")
            .expect("valid portable authority");

        let error = validate_request_for_test(&request).expect_err("unsupported on Spin");
        assert!(matches!(error, edgezero_core::EdgeError::BadRequest { .. }));
    }
}
