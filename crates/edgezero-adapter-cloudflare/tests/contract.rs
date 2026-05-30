#![cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
// Keep coverage for the deprecated low-level dispatch path while it remains
// public.
#![allow(
    deprecated,
    reason = "the deprecated dispatch helper is still part of the public API; \
              contract coverage stays until the helper is removed"
)]

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
    use bytes::Bytes;
    use edgezero_adapter_cloudflare::context::CloudflareRequestContext;
    use edgezero_adapter_cloudflare::request::{
        dispatch, dispatch_with_config, dispatch_with_config_handle, into_core_request,
    };
    use edgezero_adapter_cloudflare::response::from_core_response;
    use edgezero_core::app::App;
    use edgezero_core::body::Body;
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{response_builder, Method, Response, StatusCode};
    use edgezero_core::router::RouterService;
    use futures::stream;
    use std::sync::Arc;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use worker::js_sys::{Object, Uint8Array};
    use worker::wasm_bindgen::{JsCast as _, JsValue};
    use worker::worker_sys::Context as WorkerSysContext;
    use worker::{
        Context, Env, Headers as CfHeaders, Method as CfMethod, Request as CfRequest, RequestInit,
    };

    wasm_bindgen_test_configure!(run_in_browser);

    struct FixedConfigStore(&'static str);

    impl ConfigStore for FixedConfigStore {
        fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(Some(self.0.to_owned()))
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
            let bytes = ctx.request().body().as_bytes().expect("buffered").to_vec();
            let response = response_builder()
                .status(StatusCode::OK)
                .body(Body::from(bytes))
                .expect("response");
            Ok(response)
        }

        async fn config_presence(ctx: RequestContext) -> Result<Response, EdgeError> {
            let present = if ctx.config_store().is_some() {
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
        let mut init = RequestInit::new();
        init.with_method(method);

        let headers = CfHeaders::new();
        headers.set("host", "example.com").expect("host header");
        headers.set("x-edgezero-test", "1").expect("custom header");
        init.with_headers(headers);

        if let Some(bytes) = body {
            let array = Uint8Array::from(bytes);
            init.with_body(Some(JsValue::from(array)));
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
    async fn into_core_request_preserves_method_uri_headers_body_and_context() {
        let req = cf_request(CfMethod::Post, "/mirror?foo=bar", Some(b"payload"));
        let (env, ctx) = test_env_ctx();

        let core_request = into_core_request(req, env, ctx)
            .await
            .expect("core request");

        assert_eq!(core_request.method(), &Method::POST, "method preserved");
        assert_eq!(core_request.uri().path(), "/mirror", "uri path preserved");
        assert_eq!(
            core_request.uri().query(),
            Some("foo=bar"),
            "uri query preserved"
        );

        let header = core_request
            .headers()
            .get("x-edgezero-test")
            .and_then(|value| value.to_str().ok());
        assert_eq!(header, Some("1"), "custom header preserved");

        assert_eq!(
            core_request.body().as_bytes().expect("buffered"),
            b"payload",
            "body bytes preserved"
        );

        assert!(
            CloudflareRequestContext::get(&core_request).is_some(),
            "FastlyRequestContext attached"
        );
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

        assert_eq!(
            cf_response.status_code(),
            StatusCode::CREATED.as_u16(),
            "status code translated"
        );
        let header = cf_response
            .headers()
            .get("x-edgezero-res")
            .expect("header set");
        assert_eq!(header.as_deref(), Some("1"), "response header preserved");

        let bytes = cf_response.bytes().await.expect("bytes");
        assert_eq!(bytes.as_slice(), b"hello world", "streaming body collected");
    }

    #[wasm_bindgen_test]
    async fn dispatch_runs_router_and_returns_response() {
        let app = build_test_app();
        let req = cf_request(CfMethod::Get, "/uri", None);
        let (env, ctx) = test_env_ctx();

        let mut response = dispatch(&app, req, env, ctx).await.expect("cf response");

        assert_eq!(response.status_code(), StatusCode::OK.as_u16(), "status OK");
        let body = response.text().await.expect("text");
        assert_eq!(body, "https://example.com/uri", "echoed uri");
    }

    #[wasm_bindgen_test]
    async fn dispatch_streaming_route_preserves_chunks() {
        let app = build_test_app();
        let req = cf_request(CfMethod::Get, "/stream", None);
        let (env, ctx) = test_env_ctx();

        let mut response = dispatch(&app, req, env, ctx).await.expect("cf response");

        assert_eq!(response.status_code(), StatusCode::OK.as_u16(), "status OK");
        let bytes = response.bytes().await.expect("bytes");
        assert_eq!(bytes.as_slice(), b"chunk-1chunk-2", "chunks concatenated");
    }

    #[wasm_bindgen_test]
    async fn dispatch_passes_request_body_to_handlers() {
        let app = build_test_app();
        let req = cf_request(CfMethod::Post, "/mirror", Some(b"echo"));
        let (env, ctx) = test_env_ctx();

        let mut response = dispatch(&app, req, env, ctx).await.expect("cf response");

        assert_eq!(response.status_code(), StatusCode::OK.as_u16(), "status OK");
        let bytes = response.bytes().await.expect("bytes");
        assert_eq!(bytes.as_slice(), b"echo", "request body echoed");
    }

    #[wasm_bindgen_test]
    async fn dispatch_with_config_missing_binding_skips_injection() {
        // The test env is an empty JS object; any env.var() call returns None.
        // dispatch_with_config should log a warning and dispatch without
        // injecting a config-store handle, so the handler receives
        // ctx.config_store() == None.
        let app = build_test_app();
        let req = cf_request(CfMethod::Get, "/has-config", None);
        let (env, ctx) = test_env_ctx();

        let mut response = dispatch_with_config(&app, req, env, ctx, "nonexistent_binding")
            .await
            .expect("cf response");

        assert_eq!(response.status_code(), StatusCode::OK.as_u16(), "status OK");
        let body = response.text().await.expect("text");
        assert_eq!(body, "no", "handler observed missing config store");
    }

    #[wasm_bindgen_test]
    async fn dispatch_with_config_handle_injects_handle() {
        let app = build_test_app();
        let req = cf_request(CfMethod::Get, "/config-value", None);
        let (env, ctx) = test_env_ctx();
        let handle = ConfigStoreHandle::new(Arc::new(FixedConfigStore("hello from cf test")));

        let mut response = dispatch_with_config_handle(&app, req, env, ctx, handle)
            .await
            .expect("cf response");

        assert_eq!(response.status_code(), StatusCode::OK.as_u16(), "status OK");
        let body = response.text().await.expect("text");
        assert_eq!(body, "hello from cf test", "config value injected");
    }
}
