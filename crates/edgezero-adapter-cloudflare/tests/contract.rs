#![cfg(all(feature = "cloudflare", target_arch = "wasm32"))]

// Compile-time check: CloudflareSecretStore implements SecretStore.
mod secret_store_compile_check {
    use edgezero_adapter_cloudflare::secret_store::CloudflareSecretStore;
    use edgezero_core::secret_store::SecretStore;

    fn assert_provider_impl<T: SecretStore>() {}

    // Anonymous const whose initializer is a never-called fn pointer; the
    // type bound is checked at type-check time.
    const _: fn() = assert_provider_impl::<CloudflareSecretStore>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use edgezero_adapter_cloudflare::context::CloudflareRequestContext;
    use edgezero_adapter_cloudflare::request::{CloudflareService, into_core_request};
    use edgezero_adapter_cloudflare::response::from_core_response;
    use edgezero_core::app::{App, Hooks};
    use edgezero_core::body::Body;
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{Method, Request, Response, StatusCode, response_builder};
    use edgezero_core::request::{
        CapturedTarget, MAX_TARGET_BYTES, OriginSource, Preservation, RequestIngress,
        TargetUnavailable,
    };
    use edgezero_core::router::{PreDispatchHook, RouterService};
    use futures::stream;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use worker::js_sys::Object;
    use worker::wasm_bindgen::{JsCast as _, JsValue};
    use worker::worker_sys::Context as WorkerSysContext;
    use worker::{Context, Env, Method as CfMethod, Request as CfRequest, RequestInit};

    wasm_bindgen_test_configure!(run_in_browser);

    struct IngressHook {
        continue_routing: bool,
    }

    struct FixedConfigStore(&'static str);

    #[async_trait::async_trait(?Send)]
    impl ConfigStore for FixedConfigStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(Some(self.0.to_owned()))
        }
    }

    #[async_trait::async_trait(?Send)]
    impl PreDispatchHook for IngressHook {
        async fn handle(&self, request: &mut Request) -> Result<Option<Response>, EdgeError> {
            assert!(request.extensions().get::<RequestIngress>().is_some());
            assert_eq!(request.method().as_str(), "EXAMPLE-METHOD");
            if self.continue_routing {
                *request.method_mut() = Method::GET;
                *request.uri_mut() = "https://example.com/uri"
                    .parse()
                    .expect("should parse fixed URI");
                Ok(None)
            } else {
                Ok(Some(
                    response_builder()
                        .status(418)
                        .header("allow", "POST")
                        .body(Body::empty())
                        .map_err(EdgeError::internal)?,
                ))
            }
        }
    }

    #[wasm_bindgen_test]
    async fn ingress_preserves_runtime_extension_methods_and_header_strings() {
        for token in [
            "EXAMPLE-METHOD",
            "example-method",
            "GET",
            "POST",
            "PUT",
            "DELETE",
        ] {
            let init = web_sys::RequestInit::new();
            init.set_method(token);
            let headers = web_sys::Headers::new().expect("should create headers");
            headers
                .append("x-octets", "\u{ff}\u{80}\u{c3}\u{a9}")
                .expect("should append ByteString header");
            init.set_headers_headers(&headers);
            let raw =
                web_sys::Request::new_with_str_and_init("https://example.com/reserved", &init)
                    .expect("should create raw Web Request");
            let expected_method = raw.method();
            let (env, ctx) = test_env_ctx();
            let converted = into_core_request(CfRequest::from(raw), env, ctx)
                .await
                .expect("should convert raw request");
            assert_eq!(converted.method().as_str(), expected_method);
            assert_eq!(
                converted.headers()["x-octets"].as_bytes(),
                "\u{ff}\u{80}\u{c3}\u{a9}".as_bytes()
            );
        }
    }

    #[wasm_bindgen_test]
    async fn ingress_metadata_tracks_runtime_url_origin_and_coalescing() {
        let oversized = format!("https://example.com/{}", "a".repeat(MAX_TARGET_BYTES));
        for url in [
            "https://example.com/reserved?q=a%2Fb",
            "https://example.com/reserved/../ordinary?q=a%2Fb",
            "https://example.com/reserved/%2e%2e/ordinary",
            "https://example.com/reserved//%2F",
            oversized.as_str(),
        ] {
            let raw = web_sys::Request::new_with_str(url).expect("should construct Web Request");
            raw.headers()
                .append("x-repeat", "first")
                .expect("should append header");
            raw.headers()
                .append("x-repeat", "second")
                .expect("should append header");
            raw.headers()
                .set("origin", "https://spoof.example.com")
                .expect("should set spoofed origin");
            raw.headers()
                .set("forwarded", "proto=http;host=spoof.example.com")
                .expect("should set spoofed forwarding header");
            let exposed = raw.url();
            let (env, ctx) = test_env_ctx();
            let converted = into_core_request(CfRequest::from(raw), env, ctx)
                .await
                .expect("should convert Web Request");
            let ingress = converted
                .extensions()
                .get::<RequestIngress>()
                .expect("should insert metadata");
            match ingress.target() {
                CapturedTarget::Complete(target) => {
                    assert_eq!(target.value(), exposed);
                    assert_eq!(target.fidelity(), Preservation::Unknown);
                }
                CapturedTarget::Unavailable(reason) => {
                    assert_eq!(*reason, TargetUnavailable::TooLarge);
                }
            }
            assert_eq!(
                matches!(ingress.target(), CapturedTarget::Complete(_)),
                exposed.len() <= MAX_TARGET_BYTES
            );
            let origin = ingress.origin().expect("should expose runtime origin");
            assert_eq!(origin.scheme(), "https");
            assert_eq!(origin.authority(), "example.com");
            assert_eq!(origin.source(), OriginSource::RuntimeUri);
            assert_eq!(converted.headers().get_all("x-repeat").iter().count(), 1);
            assert_eq!(converted.headers()["x-repeat"], "first, second");
            assert_eq!(
                ingress
                    .header_fidelity(&"x-repeat".parse().expect("should parse name"))
                    .field_multiplicity(),
                Preservation::Unknown
            );
        }
    }

    #[wasm_bindgen_test]
    async fn ingress_non_http_runtime_url_has_no_origin() {
        let raw = web_sys::Request::new_with_str("ftp://example.com/reserved")
            .expect("should construct runtime URL");
        let (env, ctx) = test_env_ctx();
        let request = into_core_request(CfRequest::from(raw), env, ctx)
            .await
            .expect("should convert exposed URI");
        assert!(
            request
                .extensions()
                .get::<RequestIngress>()
                .expect("should insert metadata")
                .origin()
                .is_none()
        );
    }

    #[wasm_bindgen_test]
    async fn ingress_hook_intercepts_and_continues_through_service() {
        for continue_routing in [false, true] {
            let app = App::new(
                RouterService::builder()
                    .pre_dispatch_hook(Arc::new(IngressHook { continue_routing }))
                    .get("/uri", capture_uri_for_ingress)
                    .build(),
            );
            let init = web_sys::RequestInit::new();
            init.set_method("EXAMPLE-METHOD");
            let raw = web_sys::Request::new_with_str_and_init("https://example.com/missing", &init)
                .expect("should create extension request");
            let (env, ctx) = test_env_ctx();
            let response = CloudflareService::new(&app)
                .dispatch(CfRequest::from(raw), env, ctx)
                .await
                .expect("should dispatch hook");
            assert_eq!(
                response.status_code(),
                if continue_routing { 200 } else { 418 }
            );
            if !continue_routing {
                assert_eq!(
                    response
                        .headers()
                        .get("allow")
                        .expect("should read header")
                        .as_deref(),
                    Some("POST")
                );
            }
        }
    }

    #[edgezero_core::action]
    async fn capture_uri_for_ingress() -> &'static str {
        "continued"
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
            let bytes = ctx.request().body().as_bytes().expect("buffered").to_vec();
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

    #[wasm_bindgen_test]
    async fn dispatch_app_reuses_router_with_fresh_request_bodies() {
        struct TestApp;
        // This fixture deliberately exercises the default Hooks metadata.
        #[expect(
            clippy::missing_trait_methods,
            reason = "exercise default Hooks metadata and construction"
        )]
        impl Hooks for TestApp {
            fn routes() -> RouterService {
                build_test_app().router().clone()
            }
        }
        let app = TestApp::build_app();
        for body in [b"first".as_slice(), b"second".as_slice()] {
            let (env, ctx) = test_env_ctx();
            let request = cf_request(CfMethod::Post, "/mirror", Some(body));
            let mut response =
                edgezero_adapter_cloudflare::dispatch_app::<TestApp>(&app, request, env, ctx)
                    .await
                    .expect("prebuilt dispatch response");
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.bytes().await.expect("response bytes"), body);
        }
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

        assert_eq!(
            core_request.body().as_bytes().expect("buffered"),
            b"payload"
        );

        assert!(CloudflareRequestContext::get(&core_request).is_some());
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
}
