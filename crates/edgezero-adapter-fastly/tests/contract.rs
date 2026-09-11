// Compile-time check: FastlySecretStore implements SecretStore.
#[cfg(all(feature = "fastly", target_arch = "wasm32"))]
mod secret_store_compile_check {
    use edgezero_adapter_fastly::secret_store::FastlySecretStore;
    use edgezero_core::secret_store::SecretStore;

    fn assert_provider_impl<T: SecretStore>() {}

    // Anonymous const whose initializer is a never-called fn pointer; the
    // type bound is checked at type-check time.
    const _: fn() = assert_provider_impl::<FastlySecretStore>;
}

#[cfg(test)]
#[cfg(all(feature = "fastly", target_arch = "wasm32"))]
#[cfg_attr(
    feature = "test-utils",
    expect(
        clippy::arbitrary_source_item_ordering,
        reason = "ingress contracts are grouped after the provider fixture tests"
    )
)]
mod tests {
    use bytes::Bytes;
    use edgezero_adapter_fastly::context::FastlyRequestContext;
    use edgezero_adapter_fastly::request::{FastlyService, into_core_request};
    use edgezero_adapter_fastly::response::from_core_response;
    use edgezero_core::app::App;
    use edgezero_core::body::Body;
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{Method, Response, StatusCode, response_builder};
    use edgezero_core::router::RouterService;
    use fastly::Request as FastlyRequest;
    use fastly::http::{Method as FastlyMethod, StatusCode as FastlyStatus};
    use futures::{executor::block_on, stream};
    use std::sync::Arc;

    struct FixedConfigStore(&'static str);

    #[async_trait::async_trait(?Send)]
    #[expect(
        clippy::missing_trait_methods,
        reason = "the test provider intentionally exercises the bounded-read compatibility default"
    )]
    impl ConfigStore for FixedConfigStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(Some(self.0.to_owned()))
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
            // gone. The dispatch boundary now synthesises a one-id
            // `ConfigRegistry` from the wired `ConfigStoreHandle`.
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

        let router = RouterService::builder()
            .get("/uri", capture_uri)
            .post("/mirror", mirror_body)
            .get("/stream", stream_response)
            .get("/config", config_value)
            .build();

        App::new(router)
    }

    fn fastly_request(method: FastlyMethod, path: &str, body: Option<&[u8]>) -> FastlyRequest {
        // Viceroy validates Fastly request URLs at construction time, so the
        // contract tests must use absolute URLs instead of path-only strings.
        let mut req = FastlyRequest::new(method, format!("http://example.com{path}"));
        req.set_header("host", "example.com");
        req.set_header("x-edgezero-test", "1");
        if let Some(bytes) = body {
            req.set_body(bytes.to_vec());
        }
        req
    }

    #[test]
    fn into_core_request_preserves_method_uri_headers_body_and_context() {
        let req = fastly_request(FastlyMethod::POST, "/mirror?foo=bar", Some(b"payload"));
        let expected_ip = req.get_client_ip_addr();

        let core_request = into_core_request(req).expect("core request");

        assert_eq!(core_request.method(), &Method::POST);
        assert_eq!(core_request.uri().path(), "/mirror");
        assert_eq!(core_request.uri().query(), Some("foo=bar"));

        let headers = core_request.headers();
        assert_eq!(
            headers
                .get("x-edgezero-test")
                .and_then(|value| value.to_str().ok()),
            Some("1")
        );

        let context = FastlyRequestContext::get(&core_request).expect("context");
        assert_eq!(context.client_ip, expected_ip);

        let body =
            block_on(core_request.into_body().into_bytes_bounded(1024)).expect("request body");
        assert_eq!(body.as_ref(), b"payload");
    }

    #[test]
    fn from_core_response_translates_status_headers_and_streaming_body() {
        let response = response_builder()
            .status(StatusCode::CREATED)
            .header("x-edgezero-res", "1")
            .body(Body::stream(stream::iter(vec![
                Bytes::from_static(b"hello"),
                Bytes::from_static(b" "),
                Bytes::from_static(b"world"),
            ])))
            .expect("response");

        let mut fastly_response = from_core_response(response).expect("fastly response");

        assert_eq!(fastly_response.get_status(), FastlyStatus::CREATED);
        assert!(fastly_response.get_header("x-edgezero-res").is_some());
        assert_eq!(fastly_response.take_body_bytes(), b"hello world");
    }

    #[test]
    fn dispatch_runs_router_and_returns_response() {
        let app = build_test_app();
        let req = fastly_request(FastlyMethod::GET, "/uri", None);

        let mut response = FastlyService::new(&app)
            .dispatch(req)
            .expect("fastly response");

        assert_eq!(response.get_status(), FastlyStatus::OK);
        assert_eq!(response.take_body_bytes(), b"http://example.com/uri");
    }

    #[test]
    fn dispatch_streaming_route_preserves_chunks() {
        let app = build_test_app();
        let req = fastly_request(FastlyMethod::GET, "/stream", None);

        let mut response = FastlyService::new(&app)
            .dispatch(req)
            .expect("fastly response");

        assert_eq!(response.get_status(), FastlyStatus::OK);
        assert_eq!(response.take_body_bytes(), b"chunk-1chunk-2");
    }

    #[test]
    fn dispatch_passes_request_body_to_handlers() {
        let app = build_test_app();
        let req = fastly_request(FastlyMethod::POST, "/mirror", Some(b"echo"));

        let mut response = FastlyService::new(&app)
            .dispatch(req)
            .expect("fastly response");

        assert_eq!(response.get_status(), FastlyStatus::OK);
        assert_eq!(response.take_body_bytes(), b"echo");
    }

    #[test]
    fn service_with_config_handle_injects_handle() {
        let app = build_test_app();
        let req = fastly_request(FastlyMethod::GET, "/config", None);
        let handle = ConfigStoreHandle::new(Arc::new(FixedConfigStore("hello from fastly test")));

        let mut response = FastlyService::new(&app)
            .with_config_handle(handle)
            .dispatch(req)
            .expect("fastly response");

        assert_eq!(response.get_status(), FastlyStatus::OK);
        assert_eq!(response.take_body_bytes(), b"hello from fastly test");
    }

    #[cfg(feature = "test-utils")]
    mod ingress_contract {
        use std::collections::VecDeque;
        use std::io::{self, Read};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use edgezero_adapter_fastly::request::dispatch_ingress_reader_for_test;
        use edgezero_core::http::{HeaderMap, HeaderValue};
        use edgezero_core::ingress::{AdmissionDecision, BufferedIngressResponse, IngressGrant};
        use edgezero_core::middleware::{Middleware, Next};
        use edgezero_core::router::RouteResolution;
        use edgezero_core::time::{MonotonicClock, MonotonicInstant};

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

        struct TrackedReader {
            body_reads: Arc<AtomicUsize>,
            chunks: VecDeque<Bytes>,
            expire_on_read: Option<(Arc<Mutex<MonotonicInstant>>, MonotonicInstant)>,
            grant_drops: Arc<AtomicUsize>,
            _source_drop: DropSignal,
        }

        struct ErrorReader {
            body_reads: Arc<AtomicUsize>,
            grant_drops: Arc<AtomicUsize>,
            _source_drop: DropSignal,
        }

        #[expect(
            clippy::missing_trait_methods,
            reason = "the test source only needs the production wrapper's read method"
        )]
        impl Read for ErrorReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                if self.grant_drops.load(Ordering::SeqCst) != 0 {
                    return Err(io::Error::other("ingress grant dropped during body read"));
                }
                self.body_reads.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::other("fastly ingress source failure"))
            }
        }

        #[expect(
            clippy::missing_trait_methods,
            reason = "the test source only needs the production wrapper's read method"
        )]
        impl Read for TrackedReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.grant_drops.load(Ordering::SeqCst) != 0 {
                    return Err(io::Error::other("ingress grant dropped during body read"));
                }
                self.body_reads.fetch_add(1, Ordering::SeqCst);
                if let Some((now, deadline)) = &self.expire_on_read {
                    *now.lock()
                        .map_err(|_poisoned| io::Error::other("clock lock poisoned"))? = *deadline;
                }
                let Some(chunk) = self.chunks.pop_front() else {
                    return Ok(0);
                };
                if chunk.len() > buf.len() {
                    return Err(io::Error::other("test chunk exceeds read buffer"));
                }
                buf[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
        }

        fn tracked_reader(
            chunks: Vec<Bytes>,
            grant_drops: &Arc<AtomicUsize>,
            source_drops: &Arc<AtomicUsize>,
            body_reads: &Arc<AtomicUsize>,
            expire_on_read: Option<(Arc<Mutex<MonotonicInstant>>, MonotonicInstant)>,
        ) -> TrackedReader {
            TrackedReader {
                body_reads: Arc::clone(body_reads),
                chunks: chunks.into(),
                expire_on_read,
                grant_drops: Arc::clone(grant_drops),
                _source_drop: DropSignal(Arc::clone(source_drops)),
            }
        }

        fn error_reader(
            grant_drops: &Arc<AtomicUsize>,
            source_drops: &Arc<AtomicUsize>,
            body_reads: &Arc<AtomicUsize>,
        ) -> ErrorReader {
            ErrorReader {
                body_reads: Arc::clone(body_reads),
                grant_drops: Arc::clone(grant_drops),
                _source_drop: DropSignal(Arc::clone(source_drops)),
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
                        b"fastly overflow\0response",
                    ),
                    on_timeout: terminal_response(
                        StatusCode::GATEWAY_TIMEOUT,
                        "timeout",
                        b"fastly timeout response\n",
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

        fn assert_no_route_dispatch(handler_calls: &AtomicUsize, middleware_calls: &AtomicUsize) {
            assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
            assert_eq!(middleware_calls.load(Ordering::SeqCst), 0);
        }

        fn assert_terminal_response(
            mut response: fastly::Response,
            status: StatusCode,
            expected_headers: &HeaderMap,
            body: &[u8],
        ) {
            assert_eq!(response.get_status().as_u16(), status.as_u16());
            let mut actual_headers = HeaderMap::new();
            for name in response.get_header_names() {
                for value in response.get_header_all(name) {
                    actual_headers.append(name.clone(), value.clone());
                }
            }
            assert_eq!(&actual_headers, expected_headers);
            assert_eq!(response.take_body_bytes(), body);
        }

        #[test]
        fn exact_cap_preserves_not_found_and_method_not_allowed() {
            for (path, expected_status) in [
                ("/missing", StatusCode::NOT_FOUND),
                ("/known", StatusCode::METHOD_NOT_ALLOWED),
            ] {
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_reads = Arc::new(AtomicUsize::new(0));
                let (app, handler_calls, middleware_calls) =
                    fallback_app(4, Duration::from_secs(30), &grant_drops);
                let reader = tracked_reader(
                    vec![Bytes::from_static(b"ab"), Bytes::from_static(b"cd")],
                    &grant_drops,
                    &source_drops,
                    &body_reads,
                    None,
                );

                let response = dispatch_ingress_reader_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    reader,
                )
                .expect("response");

                assert_eq!(response.get_status().as_u16(), expected_status.as_u16());
                assert_eq!(body_reads.load(Ordering::SeqCst), 3);
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
                let body_reads = Arc::new(AtomicUsize::new(0));
                let (app, handler_calls, middleware_calls) =
                    fallback_app(4, Duration::from_secs(30), &grant_drops);
                let reader = tracked_reader(
                    vec![Bytes::from_static(b"abcd"), Bytes::from_static(b"e")],
                    &grant_drops,
                    &source_drops,
                    &body_reads,
                    None,
                );

                let response = dispatch_ingress_reader_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    reader,
                )
                .expect("response");

                assert_terminal_response(
                    response,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &terminal_headers("overflow"),
                    b"fastly overflow\0response",
                );
                assert_eq!(body_reads.load(Ordering::SeqCst), 2);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[test]
        fn standard_service_enforces_fallback_overflow() {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                fallback_app(4, Duration::from_secs(30), &grant_drops);
            let request = fastly_request(FastlyMethod::POST, "/missing", Some(b"abcde"));

            let response = FastlyService::new(&app)
                .dispatch(request)
                .expect("Fastly response");

            assert_terminal_response(
                response,
                StatusCode::UNPROCESSABLE_ENTITY,
                &terminal_headers("overflow"),
                b"fastly overflow\0response",
            );
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
        }

        #[test]
        fn cooperative_pre_read_expiry_preserves_application_response() {
            for path in ["/missing", "/known"] {
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_reads = Arc::new(AtomicUsize::new(0));
                let start = MonotonicInstant::now();
                let (mut app, handler_calls, middleware_calls) =
                    fallback_app(4, Duration::ZERO, &grant_drops);
                app.set_monotonic_clock(MonotonicClock::new(move || start));
                let reader = tracked_reader(
                    vec![Bytes::from_static(b"body")],
                    &grant_drops,
                    &source_drops,
                    &body_reads,
                    None,
                );

                let response = dispatch_ingress_reader_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    reader,
                )
                .expect("response");

                assert_terminal_response(
                    response,
                    StatusCode::GATEWAY_TIMEOUT,
                    &terminal_headers("timeout"),
                    b"fastly timeout response\n",
                );
                assert_eq!(body_reads.load(Ordering::SeqCst), 0);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[test]
        fn cooperative_post_read_expiry_preserves_application_response() {
            for path in ["/missing", "/known"] {
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_reads = Arc::new(AtomicUsize::new(0));
                let start = MonotonicInstant::now();
                let deadline = start.checked_add(Duration::from_secs(1)).expect("deadline");
                let now = Arc::new(Mutex::new(start));
                let observed_now = Arc::clone(&now);
                let (mut app, handler_calls, middleware_calls) =
                    fallback_app(4, Duration::from_secs(1), &grant_drops);
                app.set_monotonic_clock(MonotonicClock::new(move || {
                    *observed_now.lock().expect("clock lock")
                }));
                let reader = tracked_reader(
                    vec![Bytes::from_static(b"body")],
                    &grant_drops,
                    &source_drops,
                    &body_reads,
                    Some((now, deadline)),
                );

                let response = dispatch_ingress_reader_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    reader,
                )
                .expect("response");

                assert_terminal_response(
                    response,
                    StatusCode::GATEWAY_TIMEOUT,
                    &terminal_headers("timeout"),
                    b"fastly timeout response\n",
                );
                assert_eq!(body_reads.load(Ordering::SeqCst), 1);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[test]
        fn saturated_fallback_refuses_without_reading_body() {
            for path in ["/missing", "/known"] {
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_reads = Arc::new(AtomicUsize::new(0));
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
                            .body(Body::from("fastly unavailable\n"))
                            .expect("refusal response"),
                    )
                });
                let reader = tracked_reader(
                    vec![Bytes::from_static(b"body")],
                    &grant_drops,
                    &source_drops,
                    &body_reads,
                    None,
                );

                let mut response = dispatch_ingress_reader_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    reader,
                )
                .expect("response");

                let mut expected_headers = HeaderMap::new();
                expected_headers.insert("x-ingress-refusal", HeaderValue::from_static("saturated"));
                assert_eq!(
                    response.get_status().as_u16(),
                    StatusCode::SERVICE_UNAVAILABLE.as_u16()
                );
                let mut actual_headers = HeaderMap::new();
                for name in response.get_header_names() {
                    for value in response.get_header_all(name) {
                        actual_headers.append(name.clone(), value.clone());
                    }
                }
                assert_eq!(actual_headers, expected_headers);
                assert_eq!(response.take_body_bytes(), b"fastly unavailable\n");
                assert_eq!(body_reads.load(Ordering::SeqCst), 0);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 0);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        // Fastly exposes the inbound body as synchronous `Read`; there is no
        // pending future boundary at which a poll-then-drop cancellation can be
        // represented. Source-error termination is the available lifecycle seam.
        #[test]
        fn synchronous_source_error_uses_fastly_response_boundary_and_releases_lifecycle() {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let source_drops = Arc::new(AtomicUsize::new(0));
            let body_reads = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                fallback_app(4, Duration::from_secs(30), &grant_drops);
            let reader = error_reader(&grant_drops, &source_drops, &body_reads);

            let response = dispatch_ingress_reader_for_test(
                &app,
                Method::POST,
                "/missing".parse().expect("URI"),
                reader,
            )
            .expect("standard error response");

            assert_terminal_response(
                response,
                StatusCode::INTERNAL_SERVER_ERROR,
                &internal_error_headers(),
                br#"{"error":{"status":500,"kind":"internal","message":"internal server error"}}"#,
            );
            assert_eq!(body_reads.load(Ordering::SeqCst), 1);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
        }
    }
}

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod outbound_contract_tests {
    use std::cell::RefCell;
    #[cfg(target_arch = "wasm32")]
    use std::time::Duration;

    use bytes::Bytes;
    #[cfg(target_arch = "wasm32")]
    use edgezero_adapter_fastly::outbound::{FastlyOutboundClient, inject_dispatch_slack_for_test};
    use edgezero_adapter_fastly::outbound::{
        dispatch_all_before_wait_for_test, validate_batch_request_for_test,
    };
    use edgezero_core::body::Body;
    #[cfg(target_arch = "wasm32")]
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{Method, Uri};
    #[cfg(target_arch = "wasm32")]
    use edgezero_core::outbound::OutboundHttpClient as _;
    use edgezero_core::outbound::OutboundRequest;
    #[cfg(target_arch = "wasm32")]
    use futures::executor::block_on;
    use futures_util::stream;

    fn request() -> OutboundRequest {
        OutboundRequest::new(
            Method::POST,
            "https://example.com/bid".parse::<Uri>().expect("URI"),
        )
        .expect("request")
    }

    #[test]
    fn batch_preflight_rejects_streamed_slots_without_poisoning_siblings() {
        let requests = [
            request().body(Bytes::from_static(b"first")),
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

        let accepted: Vec<_> = outcomes.iter().map(Result::is_ok).collect();
        assert_eq!(accepted, vec![true, false, false, true]);
    }

    #[test]
    fn send_all_dispatches_every_slot_before_wait() {
        let events = RefCell::new(Vec::new());
        let pending = dispatch_all_before_wait_for_test(0_usize..3_usize, |index| {
            events.borrow_mut().push(format!("dispatch:{index}"));
            Ok::<_, ()>(index)
        });

        for slot in pending {
            let index = slot.expect("dispatch succeeds");
            events.borrow_mut().push(format!("wait:{index}"));
        }

        assert_eq!(
            events.into_inner(),
            [
                "dispatch:0",
                "dispatch:1",
                "dispatch:2",
                "wait:0",
                "wait:1",
                "wait:2",
            ]
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn request_preparation_consumes_entry_budget() {
        let _injection = inject_dispatch_slack_for_test(Duration::from_millis(26));
        let request = request()
            .body(Bytes::from_static(b"body"))
            .timeout(Duration::from_secs(1));

        let results = block_on(FastlyOutboundClient::new().send_all(vec![request]));

        let Err(EdgeError::Internal { source }) = &results[0].outcome else {
            panic!(
                "expected dispatch-slack failure, got {:?}",
                results[0].outcome
            );
        };
        assert_eq!(
            source.to_string(),
            "Fastly send_all adapter overhead between batch_now and SDK arming (preflight + dynamic-backend lookup/creation + SDK setup) exceeded BATCH_DISPATCH_SLACK_MAX; refusing to arm SDK timers with stale duration"
        );
    }
}
