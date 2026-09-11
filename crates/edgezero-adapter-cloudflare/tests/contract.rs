#![allow(
    clippy::expect_used,
    clippy::missing_assert_message,
    clippy::unwrap_used,
    reason = "wasm_bindgen_test functions are tests, but Clippy does not classify the generated wrappers as tests"
)]

// Compile-time check: CloudflareSecretStore implements SecretStore.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod secret_store_compile_check {
    use edgezero_adapter_cloudflare::secret_store::CloudflareSecretStore;
    use edgezero_core::secret_store::SecretStore;

    fn assert_provider_impl<T: SecretStore>() {}

    // Anonymous const whose initializer is a never-called fn pointer; the
    // type bound is checked at type-check time.
    const _: fn() = assert_provider_impl::<CloudflareSecretStore>;
}

#[cfg(all(test, feature = "cloudflare", target_arch = "wasm32"))]
#[cfg_attr(
    feature = "test-utils",
    expect(
        clippy::arbitrary_source_item_ordering,
        reason = "ingress contracts are grouped after the provider fixture tests"
    )
)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use bytes::Bytes;
    use edgezero_adapter_cloudflare::context::CloudflareRequestContext;
    use edgezero_adapter_cloudflare::outbound::CloudflareOutboundClient;
    #[cfg(feature = "test-utils")]
    use edgezero_adapter_cloudflare::outbound::{
        deferred_clock_paths_hold_for_test, response_abort_lifecycle_holds_for_test,
    };
    #[cfg(feature = "test-utils")]
    use edgezero_adapter_cloudflare::request::deadline_body_releases_source_for_test;
    use edgezero_adapter_cloudflare::request::{CloudflareService, into_core_request};
    use edgezero_adapter_cloudflare::response::from_core_response;
    #[cfg(feature = "test-utils")]
    use edgezero_adapter_cloudflare::response::response_write_deadline_uses_injected_clock_for_test;
    use edgezero_core::app::App;
    use edgezero_core::body::Body;
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{Method, Response, StatusCode, response_builder};
    use edgezero_core::outbound::{OutboundHttpClient as _, OutboundRequest};
    use edgezero_core::router::RouterService;
    use edgezero_core::time::{MonotonicClock, MonotonicInstant};
    use futures::stream;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use worker::js_sys::Object;
    use worker::wasm_bindgen::{JsCast as _, JsValue};
    use worker::worker_sys::Context as WorkerSysContext;
    use worker::{Context, Env, Method as CfMethod, Request as CfRequest, RequestInit};

    wasm_bindgen_test_configure!(run_in_browser);

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

        async fn config_presence(ctx: RequestContext) -> Result<Response, EdgeError> {
            // Hard-cutoff: legacy `ctx.config_handle()` is
            // gone. The dispatch boundary now synthesises a one-id
            // `ConfigRegistry` from the wired `ConfigStoreHandle`, so
            // the registry-aware accessor resolves the same store.
            let present = if ctx.config_store_default().is_some() {
                "yes"
            } else {
                "no"
            };
            let response = response_builder()
                .status(StatusCode::OK)
                .body(Body::text(present))
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
            // gone. See `config_presence` for the migration rationale.
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
            .get("/has-config", config_presence)
            .get("/config-value", config_value)
            .build();

        App::new(router)
    }

    fn cf_request(method: CfMethod, path: &str, body: Option<&[u8]>) -> CfRequest {
        use worker::js_sys::Uint8Array;

        let mut init = RequestInit::new();
        init.with_method(method);

        let headers = worker::Headers::new();
        headers.set("host", "example.com").expect("host header");
        headers.set("x-edgezero-test", "1").expect("custom header");
        init.with_headers(headers);

        if let Some(bytes) = body {
            let array = Uint8Array::from(bytes);
            init.with_body(Some(JsValue::from(array))); // Uint8Array -> JsValue
        }

        let url = format!("https://example.com{path}");
        CfRequest::new_with_init(&url, &init).expect("cf request")
    }

    fn test_env_ctx() -> (Env, Context) {
        let env = Object::new().unchecked_into::<Env>();
        let js_context = Object::new().unchecked_into::<WorkerSysContext>();
        (env, Context::new(js_context))
    }

    #[cfg(feature = "test-utils")]
    #[wasm_bindgen_test]
    fn deadline_body_releases_source_when_timeout_is_emitted() {
        assert!(deadline_body_releases_source_for_test());
    }

    #[wasm_bindgen_test]
    async fn dispatch_passes_request_body_to_handlers() {
        let app = build_test_app();
        let req = cf_request(CfMethod::Post, "/mirror", Some(b"echo"));
        let (env, ctx) = test_env_ctx();

        let mut response = CloudflareService::new(&app)
            .dispatch(req, env, ctx)
            .await
            .expect("cf response");

        assert_eq!(response.status_code(), StatusCode::OK.as_u16());
        let bytes = response.bytes().await.expect("bytes");
        assert_eq!(bytes.as_slice(), b"echo");
    }

    #[wasm_bindgen_test]
    async fn dispatch_runs_router_and_returns_response() {
        let app = build_test_app();
        let req = cf_request(CfMethod::Get, "/uri", None);
        let (env, ctx) = test_env_ctx();

        let mut response = CloudflareService::new(&app)
            .dispatch(req, env, ctx)
            .await
            .expect("cf response");

        assert_eq!(response.status_code(), StatusCode::OK.as_u16());
        let body = response.text().await.expect("text");
        assert_eq!(body, "https://example.com/uri");
    }

    #[wasm_bindgen_test]
    async fn dispatch_streaming_route_preserves_chunks() {
        let app = build_test_app();
        let req = cf_request(CfMethod::Get, "/stream", None);
        let (env, ctx) = test_env_ctx();

        let mut response = CloudflareService::new(&app)
            .dispatch(req, env, ctx)
            .await
            .expect("cf response");

        assert_eq!(response.status_code(), StatusCode::OK.as_u16());
        let bytes = response.bytes().await.expect("bytes");
        assert_eq!(bytes.as_slice(), b"chunk-1chunk-2");
    }

    #[wasm_bindgen_test]
    async fn from_core_response_translates_status_headers_and_streaming_body() {
        let response = response_builder()
            .status(StatusCode::CREATED)
            .header("x-edgezero-res", "1")
            .body(Body::stream(stream::iter(vec![
                Bytes::from_static(b"hello"),
                Bytes::from_static(b" "),
                Bytes::from_static(b"world"),
            ])))
            .expect("response");

        let mut cf_response = from_core_response(response).expect("cf response");

        assert_eq!(cf_response.status_code(), StatusCode::CREATED.as_u16());
        let header = cf_response.headers().get("x-edgezero-res").unwrap();
        assert_eq!(header.as_deref(), Some("1"));

        let bytes = cf_response.bytes().await.expect("bytes");
        assert_eq!(bytes.as_slice(), b"hello world");
    }

    #[wasm_bindgen_test]
    async fn into_core_request_preserves_method_uri_headers_body_and_context() {
        let req = cf_request(CfMethod::Post, "/mirror?foo=bar", Some(b"payload"));
        let (env, ctx) = test_env_ctx();

        let core_request = into_core_request(req, env, ctx)
            .await
            .expect("core request");

        assert_eq!(core_request.method(), &Method::POST);
        assert_eq!(core_request.uri().path(), "/mirror");
        assert_eq!(core_request.uri().query(), Some("foo=bar"));

        let header = core_request
            .headers()
            .get("x-edgezero-test")
            .and_then(|value| value.to_str().ok());
        assert_eq!(header, Some("1"));

        assert!(CloudflareRequestContext::get(&core_request).is_some());

        let body = core_request
            .into_body()
            .into_bytes_bounded(1024)
            .await
            .expect("request body");
        assert_eq!(body.as_ref(), b"payload");
    }

    #[wasm_bindgen_test]
    async fn service_with_config_handle_injects_handle() {
        let app = build_test_app();
        let req = cf_request(CfMethod::Get, "/config-value", None);
        let (env, ctx) = test_env_ctx();
        let handle = ConfigStoreHandle::new(Arc::new(FixedConfigStore("hello from cf test")));

        let mut response = CloudflareService::new(&app)
            .with_config_handle(handle)
            .dispatch(req, env, ctx)
            .await
            .expect("cf response");

        assert_eq!(response.status_code(), StatusCode::OK.as_u16());
        let body = response.text().await.expect("text");
        assert_eq!(body, "hello from cf test");
    }

    #[wasm_bindgen_test]
    async fn service_with_config_missing_binding_skips_injection() {
        // The test env is an empty JS object; any env.var() call returns None.
        // `CloudflareService::with_config(name)` should log a warning and
        // dispatch without injecting a config-store handle, so the handler
        // sees `ctx.config_store_default()` return `None`.
        let app = build_test_app();
        let req = cf_request(CfMethod::Get, "/has-config", None);
        let (env, ctx) = test_env_ctx();

        let mut response = CloudflareService::new(&app)
            .with_config("nonexistent_binding")
            .dispatch(req, env, ctx)
            .await
            .expect("cf response");

        assert_eq!(response.status_code(), StatusCode::OK.as_u16());
        let body = response.text().await.expect("text");
        assert_eq!(body, "no");
    }

    #[wasm_bindgen_test]
    async fn outbound_clock_controls_preflight_elapsed_and_backwards_failure() {
        use std::collections::VecDeque;
        use std::sync::Mutex;

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

        let start = MonotonicInstant::now();
        let completed = start
            .checked_add(Duration::from_millis(9))
            .expect("completed instant");
        let forward_client =
            CloudflareOutboundClient::with_clock(scripted_clock(vec![start, completed]));
        let forward_request = OutboundRequest::get("https://example.com/")
            .expect("request")
            .stream_response();
        let forward_results = forward_client.send_all(vec![forward_request]).await;
        assert_eq!(forward_results.len(), 1);
        let forward_result = forward_results.first().expect("single forward result");
        assert_eq!(forward_result.elapsed, Duration::from_millis(9));
        assert!(matches!(
            forward_result.outcome,
            Err(EdgeError::BadRequest { .. })
        ));

        let earlier = start
            .checked_sub(Duration::from_millis(1))
            .expect("earlier instant");
        let backwards_client =
            CloudflareOutboundClient::with_clock(scripted_clock(vec![start, earlier]));
        let backwards_request = OutboundRequest::get("https://example.com/")
            .expect("request")
            .stream_response();
        let backwards_results = backwards_client.send_all(vec![backwards_request]).await;
        assert_eq!(backwards_results.len(), 1);
        let backwards_result = backwards_results.first().expect("single backwards result");
        assert_eq!(backwards_result.elapsed, Duration::ZERO);
        assert!(matches!(
            backwards_result.outcome,
            Err(EdgeError::Internal { .. })
        ));
    }

    #[wasm_bindgen_test]
    async fn send_all_preserves_three_preflight_slots_in_input_order() {
        let now = MonotonicInstant::now();
        let client = CloudflareOutboundClient::with_clock(MonotonicClock::new(move || now));
        let streamed_upload = OutboundRequest::post("https://example.com/upload")
            .expect("upload request")
            .body(Body::stream(stream::iter([Bytes::from_static(b"body")])));
        let streamed_response = OutboundRequest::get("https://example.com/stream")
            .expect("stream request")
            .stream_response();
        let method_error = OutboundRequest::get("https://example.com/get")
            .expect("GET request")
            .body(Body::stream(stream::iter([Bytes::new()])));

        let results = client
            .send_all(vec![streamed_upload, streamed_response, method_error])
            .await;
        let messages: Vec<_> = results
            .iter()
            .map(|slot| match &slot.outcome {
                Err(EdgeError::BadRequest { message }) => Some(message.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(
            messages,
            [
                Some("send_all requires buffered request bodies; use send for a streamed upload"),
                Some("send_all requires buffered responses; use send for a streamed response"),
                Some(
                    "GET/HEAD request must not carry a streamed body; emptiness cannot be determined without consuming the stream"
                ),
            ]
        );
        assert!(results.iter().all(|slot| slot.elapsed == Duration::ZERO));
    }

    #[wasm_bindgen_test]
    async fn standard_service_installs_the_application_outbound_clock() {
        async fn elapsed(ctx: RequestContext) -> Result<String, EdgeError> {
            let client = ctx
                .http_client()
                .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("missing HTTP client")))?;
            let request = OutboundRequest::get("https://example.com/")?.stream_response();
            let results = client.send_all(vec![request]).await;
            let result = results
                .first()
                .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("missing outbound result")))?;
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
        let req = cf_request(CfMethod::Get, "/clock", None);
        let (env, ctx) = test_env_ctx();

        let mut response = CloudflareService::new(&app)
            .dispatch(req, env, ctx)
            .await
            .expect("Cloudflare response");

        assert_eq!(response.text().await.expect("response body"), "7");
        assert!(observations.load(Ordering::SeqCst) >= 3);
    }

    #[cfg(feature = "test-utils")]
    #[wasm_bindgen_test]
    async fn deferred_upload_and_response_paths_retain_the_injected_clock() {
        assert!(deferred_clock_paths_hold_for_test().await);
    }

    #[cfg(feature = "test-utils")]
    #[wasm_bindgen_test]
    async fn response_abort_and_write_deadline_lifecycles_are_enforced() {
        assert!(response_abort_lifecycle_holds_for_test().await);
        assert!(response_write_deadline_uses_injected_clock_for_test().await);
    }

    #[cfg(feature = "test-utils")]
    mod ingress_contract {
        use std::future::Future as _;
        use std::io;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::Poll;
        use std::time::Duration;

        use edgezero_adapter_cloudflare::request::dispatch_ingress_stream_for_test;
        use edgezero_core::http::{HeaderMap, HeaderValue};
        use edgezero_core::ingress::{AdmissionDecision, BufferedIngressResponse, IngressGrant};
        use edgezero_core::middleware::{Middleware, Next};
        use edgezero_core::router::RouteResolution;
        use futures::future::poll_fn as poll_future;
        use futures::stream::poll_fn;
        use worker::{Delay, Response as CfResponse};

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
                        b"cloudflare overflow\0response",
                    ),
                    on_timeout: terminal_response(
                        StatusCode::GATEWAY_TIMEOUT,
                        "timeout",
                        b"cloudflare timeout response\n",
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
        ) -> impl futures::Stream<Item = Result<Bytes, io::Error>> + 'static {
            let observed_grant_drops = Arc::clone(grant_drops);
            let observed_body_polls = Arc::clone(body_polls);
            let source_drop = DropSignal(Arc::clone(source_drops));
            let mut pending_chunks = chunks.into_iter();
            poll_fn(move |_cx| {
                let _keep_source_alive = &source_drop;
                assert_eq!(observed_grant_drops.load(Ordering::SeqCst), 0);
                observed_body_polls.fetch_add(1, Ordering::SeqCst);
                Poll::Ready(pending_chunks.next().map(Ok))
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
                    Poll::Ready(Some(Err(io::Error::other(
                        "cloudflare ingress source failure",
                    ))))
                }
            })
        }

        fn assert_no_route_dispatch(handler_calls: &AtomicUsize, middleware_calls: &AtomicUsize) {
            assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
            assert_eq!(middleware_calls.load(Ordering::SeqCst), 0);
        }

        async fn assert_terminal_response(
            mut response: CfResponse,
            status: StatusCode,
            expected_headers: HeaderMap,
            body: &[u8],
        ) {
            assert_eq!(response.status_code(), status.as_u16());
            let mut actual_headers = response.headers().entries().collect::<Vec<_>>();
            actual_headers.sort_unstable();
            let mut expected_provider_headers = expected_headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        value.to_str().expect("UTF-8 test header").to_owned(),
                    )
                })
                .collect::<Vec<_>>();
            expected_provider_headers.sort_unstable();
            assert_eq!(actual_headers, expected_provider_headers);
            assert_eq!(response.bytes().await.expect("provider body"), body);
        }

        #[wasm_bindgen_test]
        async fn exact_cap_preserves_not_found_and_method_not_allowed() {
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
                );

                let response = dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                )
                .await
                .expect("response");

                assert_eq!(response.status_code(), expected_status.as_u16());
                assert_eq!(body_polls.load(Ordering::SeqCst), 3);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[wasm_bindgen_test]
        async fn cap_plus_one_precedes_not_found_and_method_not_allowed() {
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
                );

                let response = dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                )
                .await
                .expect("response");

                assert_terminal_response(
                    response,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    terminal_headers("overflow"),
                    b"cloudflare overflow\0response",
                )
                .await;
                assert_eq!(body_polls.load(Ordering::SeqCst), 2);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[wasm_bindgen_test]
        async fn standard_service_enforces_fallback_overflow() {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                fallback_app(4, Duration::from_secs(30), &grant_drops);
            let req = cf_request(CfMethod::Post, "/missing", Some(b"abcde"));
            let (env, ctx) = test_env_ctx();

            let response = CloudflareService::new(&app)
                .dispatch(req, env, ctx)
                .await
                .expect("Cloudflare response");

            assert_terminal_response(
                response,
                StatusCode::UNPROCESSABLE_ENTITY,
                terminal_headers("overflow"),
                b"cloudflare overflow\0response",
            )
            .await;
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
        }

        #[wasm_bindgen_test]
        async fn wasm_timer_timeout_preserves_application_response() {
            for path in ["/missing", "/known"] {
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let body_polls = Arc::new(AtomicUsize::new(0));
                let (app, handler_calls, middleware_calls) =
                    fallback_app(4, Duration::from_millis(100), &grant_drops);
                let source = pending_stream(&grant_drops, &source_drops, &body_polls);

                let response = dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                )
                .await
                .expect("response");

                assert_terminal_response(
                    response,
                    StatusCode::GATEWAY_TIMEOUT,
                    terminal_headers("timeout"),
                    b"cloudflare timeout response\n",
                )
                .await;
                assert!(body_polls.load(Ordering::SeqCst) > 0);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[wasm_bindgen_test]
        async fn saturated_fallback_refuses_without_polling_body() {
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
                            .body(Body::from("cloudflare unavailable\n"))
                            .expect("refusal response"),
                    )
                });
                let unobserved_grant = Arc::new(AtomicUsize::new(0));
                let source = pending_stream(&unobserved_grant, &source_drops, &body_polls);

                let response = dispatch_ingress_stream_for_test(
                    &app,
                    Method::POST,
                    path.parse().expect("URI"),
                    source,
                )
                .await
                .expect("response");

                assert_terminal_response(
                    response,
                    StatusCode::SERVICE_UNAVAILABLE,
                    {
                        let mut headers = HeaderMap::new();
                        headers.insert("x-ingress-refusal", HeaderValue::from_static("saturated"));
                        headers
                    },
                    b"cloudflare unavailable\n",
                )
                .await;
                assert_eq!(body_polls.load(Ordering::SeqCst), 0);
                assert_eq!(unobserved_grant.load(Ordering::SeqCst), 0);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_no_route_dispatch(&handler_calls, &middleware_calls);
            }
        }

        #[wasm_bindgen_test]
        async fn source_error_uses_worker_response_boundary_and_releases_lifecycle() {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let source_drops = Arc::new(AtomicUsize::new(0));
            let body_polls = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                fallback_app(4, Duration::from_secs(30), &grant_drops);
            let source = error_stream(&grant_drops, &source_drops, &body_polls);

            let response = dispatch_ingress_stream_for_test(
                &app,
                Method::POST,
                "/missing".parse().expect("URI"),
                source,
            )
            .await
            .expect("standard error response");

            assert_terminal_response(
                response,
                StatusCode::INTERNAL_SERVER_ERROR,
                internal_error_headers(),
                br#"{"error":{"kind":"internal","message":"internal server error","status":500}}"#,
            )
            .await;
            assert_eq!(body_polls.load(Ordering::SeqCst), 1);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
        }

        #[wasm_bindgen_test]
        async fn poll_then_drop_releases_pending_source_and_grant() {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let source_drops = Arc::new(AtomicUsize::new(0));
            let body_polls = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                fallback_app(4, Duration::from_secs(30), &grant_drops);
            let source = pending_stream(&grant_drops, &source_drops, &body_polls);
            let mut dispatch = Box::pin(dispatch_ingress_stream_for_test(
                &app,
                Method::POST,
                "/missing".parse().expect("URI"),
                source,
            ));

            let first_poll = poll_future(|cx| Poll::Ready(dispatch.as_mut().poll(cx))).await;
            assert!(first_poll.is_pending());
            Delay::from(Duration::ZERO).await;
            let source_poll = poll_future(|cx| Poll::Ready(dispatch.as_mut().poll(cx))).await;
            assert!(source_poll.is_pending());
            assert!(body_polls.load(Ordering::SeqCst) > 0);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 0);
            assert_eq!(source_drops.load(Ordering::SeqCst), 0);

            drop(dispatch);

            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
            assert_no_route_dispatch(&handler_calls, &middleware_calls);
        }
    }
}

#[cfg(test)]
mod native_tests {
    #[cfg(all(feature = "test-utils", not(target_arch = "wasm32")))]
    mod enabled {
        use bytes::Bytes;
        use edgezero_adapter_cloudflare::outbound::{
            generic_fetch_failure_for_test, timeout_error_for_test, validate_batch_request_for_test,
        };
        use edgezero_core::body::Body;
        use edgezero_core::error::{BadGatewayReason, BudgetSource, EdgeError};
        use edgezero_core::outbound::OutboundRequest;
        use futures_util::stream;

        fn message(result: Result<(), EdgeError>) -> Option<String> {
            if let Err(EdgeError::BadRequest { message }) = result {
                Some(message)
            } else {
                None
            }
        }

        #[test]
        fn send_all_preflight_precedence_and_indices() {
            let streamed_upload = OutboundRequest::post("https://example.com/upload")
                .expect("request")
                .body(Body::stream(stream::iter([Bytes::from_static(b"body")])));
            let streamed_response = OutboundRequest::get("https://example.com/response")
                .expect("request")
                .stream_response();
            let get_stream = OutboundRequest::get("https://example.com/get")
                .expect("request")
                .body(Body::stream(stream::iter([Bytes::new()])));

            assert_eq!(
                [streamed_upload, streamed_response, get_stream]
                    .iter()
                    .map(|request| message(validate_batch_request_for_test(request)))
                    .collect::<Vec<_>>(),
                [
                    Some("send_all requires buffered request bodies; use send for a streamed upload".to_owned()),
                    Some("send_all requires buffered responses; use send for a streamed response".to_owned()),
                    Some("GET/HEAD request must not carry a streamed body; emptiness cannot be determined without consuming the stream".to_owned()),
                ]
            );
        }

        #[test]
        fn opaque_fetch_rejection_does_not_claim_connection_phase_evidence() {
            let error = generic_fetch_failure_for_test();
            assert!(matches!(
                error,
                EdgeError::BadGateway {
                    reason: BadGatewayReason::Unspecified,
                    ..
                }
            ));
        }

        #[test]
        fn worker_budget_timeouts_preserve_every_selected_source() {
            for selected in [
                BudgetSource::PerCallTimeout,
                BudgetSource::BatchDeadline,
                BudgetSource::Default,
            ] {
                assert!(matches!(
                    timeout_error_for_test(selected),
                    EdgeError::GatewayTimeout { cause, .. } if cause == selected
                ));
            }
        }
    }
}
