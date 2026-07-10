use crate::body::Body;
use crate::error::EdgeError;
use crate::http::Request;
use crate::params::PathParams;
use crate::proxy::ProxyHandle;
use crate::store_registry::{
    BoundConfigStore, BoundKvStore, BoundSecretStore, ConfigRegistry, ConfigStoreBinding,
    KvRegistry, SecretRegistry, StoreRegistry,
};
use serde::de::DeserializeOwned;

/// Request context exposed to handlers and middleware.
pub struct RequestContext {
    path_params: PathParams,
    request: Request,
}

impl RequestContext {
    #[inline]
    pub fn body(&self) -> &Body {
        self.request.body()
    }

    /// Resolve the [`BoundConfigStore`] for `id`. Strict lookup: when a
    /// [`ConfigRegistry`] is wired, an unregistered id yields `None`. When
    /// no registry is wired this returns `None` — adapter dispatchers
    /// normalise legacy bare-handle inputs to a single-id registry under
    /// the conventional `"default"` id, so a missing registry is a real
    /// bug rather than a hand-wired single-handle adapter (spec hard-cutoff).
    #[inline]
    pub fn config_store(&self, id: &str) -> Option<BoundConfigStore> {
        self.request
            .extensions()
            .get::<ConfigRegistry>()
            .and_then(|registry| registry.named(id))
            .map(|binding| binding.handle)
    }

    /// Borrow a named binding.
    #[must_use]
    #[inline]
    pub fn config_store_binding(&self, id: &str) -> Option<&ConfigStoreBinding> {
        self.request
            .extensions()
            .get::<ConfigRegistry>()
            .and_then(|registry| registry.named_ref(id))
    }

    /// Resolve the default [`BoundConfigStore`] — the wired registry's
    /// declared default id, or `None` when no registry is in extensions.
    /// See [`Self::config_store`] for the hard-cutoff rationale.
    #[inline]
    pub fn config_store_default(&self) -> Option<BoundConfigStore> {
        self.request
            .extensions()
            .get::<ConfigRegistry>()
            .and_then(StoreRegistry::default)
            .map(|binding| binding.handle)
    }

    /// Borrow the default config-store binding (handle + key). See
    /// spec 5.2.1.
    #[must_use]
    #[inline]
    pub fn config_store_default_binding(&self) -> Option<&ConfigStoreBinding> {
        self.request
            .extensions()
            .get::<ConfigRegistry>()
            .and_then(|registry| registry.default_ref())
    }

    /// Clone a request extension of type `T`, if present. Used by the
    /// introspection extractors (`ManifestJson` / `RouteTable`) to read the
    /// payload the router injected for their route.
    #[must_use]
    #[inline]
    pub(crate) fn extension<T>(&self) -> Option<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        self.request.extensions().get::<T>().cloned()
    }

    /// # Errors
    /// Returns [`EdgeError::bad_request`] if the body cannot be deserialized as form-urlencoded data into `T`, or the body is streaming.
    #[inline]
    pub fn form<T>(&self) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        match self.request.body() {
            Body::Once(bytes) => serde_urlencoded::from_bytes(bytes.as_ref())
                .map_err(|err| EdgeError::bad_request(format!("invalid form payload: {err}"))),
            Body::Stream(_) => Err(EdgeError::bad_request(
                "streaming bodies are not supported for form extraction",
            )),
        }
    }

    #[inline]
    pub fn into_request(self) -> Request {
        self.request
    }

    /// # Errors
    /// Returns [`EdgeError::bad_request`] if the body is not valid JSON for `T`.
    #[inline]
    pub fn json<T>(&self) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        self.request
            .body()
            .to_json()
            .map_err(|err| EdgeError::bad_request(format!("invalid JSON payload: {err}")))
    }

    /// Resolve the [`BoundKvStore`] for `id`. Strict lookup: when a
    /// [`KvRegistry`] is wired, an unregistered id yields `None`. When no
    /// registry is wired this returns `None` — adapter dispatchers
    /// normalise legacy bare-handle inputs to a single-id registry under
    /// the conventional `"default"` id (spec hard-cutoff).
    #[inline]
    pub fn kv_store(&self, id: &str) -> Option<BoundKvStore> {
        self.request
            .extensions()
            .get::<KvRegistry>()
            .and_then(|registry| registry.named(id))
    }

    /// Resolve the default [`BoundKvStore`] — the wired registry's
    /// declared default id, or `None` when no registry is in extensions.
    /// See [`Self::kv_store`] for the hard-cutoff rationale.
    #[inline]
    pub fn kv_store_default(&self) -> Option<BoundKvStore> {
        self.request
            .extensions()
            .get::<KvRegistry>()
            .and_then(StoreRegistry::default)
    }

    #[inline]
    pub fn new(request: Request, params: PathParams) -> Self {
        Self {
            path_params: params,
            request,
        }
    }

    /// # Errors
    /// Returns [`EdgeError::bad_request`] if the path parameters cannot be deserialized into `T`.
    #[inline]
    pub fn path<T>(&self) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        self.path_params
            .deserialize()
            .map_err(|err| EdgeError::bad_request(format!("invalid path parameters: {err}")))
    }

    #[inline]
    pub fn path_params(&self) -> &PathParams {
        &self.path_params
    }

    #[inline]
    pub fn proxy_handle(&self) -> Option<ProxyHandle> {
        self.request.extensions().get::<ProxyHandle>().cloned()
    }

    /// # Errors
    /// Returns [`EdgeError::bad_request`] if the query string cannot be deserialized into `T`.
    #[inline]
    pub fn query<T>(&self) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        let query = self.request.uri().query().unwrap_or("");
        serde_urlencoded::from_str(query)
            .map_err(|err| EdgeError::bad_request(format!("invalid query string: {err}")))
    }

    #[inline]
    pub fn request(&self) -> &Request {
        &self.request
    }

    #[inline]
    pub fn request_mut(&mut self) -> &mut Request {
        &mut self.request
    }

    /// Resolve the [`BoundSecretStore`] for `id`. Strict lookup: when a
    /// [`SecretRegistry`] is wired, an unregistered id yields `None`.
    /// When no registry is wired this returns `None` — adapter
    /// dispatchers normalise legacy bare-handle inputs to a single-id
    /// registry under the conventional `"default"` id (spec hard-cutoff).
    #[inline]
    pub fn secret_store(&self, id: &str) -> Option<BoundSecretStore> {
        self.request
            .extensions()
            .get::<SecretRegistry>()
            .and_then(|registry| registry.named(id))
    }

    /// Resolve the default [`BoundSecretStore`] — the wired registry's
    /// declared default id, or `None` when no registry is in extensions.
    /// See [`Self::secret_store`] for the hard-cutoff rationale.
    #[inline]
    pub fn secret_store_default(&self) -> Option<BoundSecretStore> {
        self.request
            .extensions()
            .get::<SecretRegistry>()
            .and_then(StoreRegistry::default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{request_builder, HeaderValue, Method, StatusCode, Uri};
    use crate::params::PathParams;
    use crate::proxy::{ProxyClient, ProxyHandle, ProxyRequest, ProxyResponse};
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::executor::block_on;
    use futures::stream;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    struct DummyClient;

    #[derive(Debug, PartialEq, Deserialize, Serialize)]
    struct PathData {
        id: String,
    }

    #[async_trait(?Send)]
    impl ProxyClient for DummyClient {
        async fn send(&self, _request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
            Ok(ProxyResponse::new(StatusCode::OK, Body::empty()))
        }
    }

    fn ctx(path: &str, body: Body, params: PathParams) -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri(path)
            .body(body)
            .expect("request");
        RequestContext::new(request, params)
    }

    fn params(map: &[(&str, &str)]) -> PathParams {
        let inner = map
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>();
        PathParams::new(inner)
    }

    // `RequestContext::config_handle()` was removed. The
    // present/absent behaviour is now covered by
    // `config_store_*` tests against a wired `ConfigRegistry`.

    #[test]
    fn form_deserialises_successfully() {
        #[derive(Deserialize, PartialEq, Debug)]
        struct FormData {
            name: String,
        }
        let body = Body::from("name=demo");
        let ctx = ctx("/submit", body, PathParams::default());
        let parsed: FormData = ctx.form().expect("form data");
        assert_eq!(
            parsed,
            FormData {
                name: "demo".into()
            }
        );
        let debug = format!("{parsed:?}");
        assert!(debug.contains("demo"));
    }

    #[test]
    fn form_streaming_body_not_supported() {
        let stream = stream::iter(vec![Ok::<Bytes, anyhow::Error>(Bytes::from("name=demo"))]);
        let body = Body::from_stream(stream);
        let ctx = ctx("/submit", body, PathParams::default());
        let err = ctx.form::<serde_json::Value>().expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err
            .message()
            .contains("streaming bodies are not supported for form extraction"));
    }

    #[test]
    fn form_value_deserialises_successfully() {
        let body = Body::from("name=demo");
        let ctx = ctx("/submit", body, PathParams::default());
        let parsed: serde_json::Value = ctx.form().expect("form data");
        assert_eq!(
            parsed.get("name").and_then(|value| value.as_str()),
            Some("demo")
        );
    }

    #[test]
    fn invalid_form_returns_bad_request() {
        #[expect(dead_code, reason = "field exercised only via Deserialize")]
        #[derive(Deserialize)]
        struct FormData {
            age: u8,
        }
        let body = Body::from("age=not-a-number");
        let ctx = ctx("/submit", body, PathParams::default());
        let err = ctx.form::<FormData>().err().expect("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid form payload"));
    }

    #[test]
    fn invalid_json_returns_bad_request() {
        let body = Body::from(&b"not json"[..]);
        let ctx = ctx("/echo", body, PathParams::default());
        let err = ctx.json::<serde_json::Value>().expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid JSON payload"));
    }

    #[test]
    fn invalid_path_returns_bad_request() {
        #[expect(dead_code, reason = "field exercised only via Deserialize")]
        #[derive(Debug, Deserialize)]
        struct NumericPath {
            id: u32,
        }
        let debug = format!("{:?}", NumericPath { id: 0 });
        assert!(debug.contains('0'));
        let ctx = ctx("/items/foo", Body::empty(), params(&[("id", "foo")]));
        let err = ctx.path::<NumericPath>().expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid path parameters"));
    }

    #[test]
    fn invalid_query_returns_bad_request() {
        #[expect(dead_code, reason = "field exercised only via Deserialize")]
        #[derive(Debug, Deserialize)]
        struct Query {
            page: u8,
        }
        let debug = format!("{:?}", Query { page: 0 });
        assert!(debug.contains('0'));
        let ctx = ctx("/items?page=foo", Body::empty(), PathParams::default());
        let err = ctx.query::<Query>().expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid query string"));
    }

    #[test]
    fn json_deserialises_from_body() {
        #[derive(Debug, Deserialize, Serialize, PartialEq)]
        struct Payload {
            name: String,
        }
        let body = Body::json(&Payload {
            name: "demo".into(),
        })
        .expect("json body");
        let ctx = ctx("/echo", body, PathParams::default());
        let parsed: Payload = ctx.json().expect("json payload");
        assert_eq!(
            parsed,
            Payload {
                name: "demo".into()
            }
        );
    }

    // `RequestContext::kv_handle()` was removed. The
    // present/absent behaviour is now covered by `kv_store_*`
    // tests against a wired `KvRegistry`.

    #[test]
    fn path_deserialises_successfully() {
        let ctx = ctx("/items/42", Body::empty(), params(&[("id", "42")]));
        let parsed: PathData = ctx.path().expect("path parameters");
        assert_eq!(parsed, PathData { id: "42".into() });
        let serialized = serde_json::to_string(&parsed).expect("serialize");
        assert!(serialized.contains("42"));
    }

    #[test]
    fn proxy_handle_forwards_with_dummy_client() {
        let handle = ProxyHandle::with_client(DummyClient);
        let request = ProxyRequest::new(Method::GET, Uri::from_static("https://example.com"));
        let response = block_on(handle.forward(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn proxy_handle_is_retrieved_when_present() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/proxy")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(ProxyHandle::with_client(DummyClient));

        let ctx = RequestContext::new(request, PathParams::default());
        assert!(ctx.proxy_handle().is_some());
    }

    #[test]
    fn query_defaults_to_empty_when_missing() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Query {
            page: Option<u8>,
        }
        let ctx = ctx("/items", Body::empty(), PathParams::default());
        let parsed: Query = ctx.query().expect("query");
        assert_eq!(parsed.page, None);
    }

    #[test]
    fn query_deserialises_successfully() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Query {
            page: u8,
        }
        let ctx = ctx("/items?page=5", Body::empty(), PathParams::default());
        let parsed: Query = ctx.query().expect("query");
        assert_eq!(parsed, Query { page: 5 });
    }

    #[test]
    fn request_context_accessors_return_expected_values() {
        let mut ctx = ctx(
            "/items/123",
            Body::from("payload"),
            params(&[("id", "123")]),
        );
        assert_eq!(ctx.request().uri().path(), "/items/123");
        ctx.request_mut()
            .headers_mut()
            .insert("x-test", HeaderValue::from_static("value"));
        assert_eq!(
            ctx.request()
                .headers()
                .get("x-test")
                .and_then(|value| value.to_str().ok()),
            Some("value")
        );
        assert_eq!(ctx.path_params().get("id"), Some("123"));
        assert_eq!(ctx.body().as_bytes().expect("buffered"), b"payload");

        let request = ctx.into_request();
        assert_eq!(request.uri().path(), "/items/123");
    }

    // `RequestContext::secret_handle()` was removed. The
    // present/absent behaviour is now covered by `secret_store_*`
    // tests against a wired `SecretRegistry`.

    #[test]
    fn kv_store_resolves_named_handle_from_registry() {
        use crate::key_value_store::{KvHandle, NoopKvStore};
        use crate::store_registry::{KvRegistry, StoreRegistry};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let sessions = KvHandle::new(Arc::new(NoopKvStore));
        let cache = KvHandle::new(Arc::new(NoopKvStore));
        let by_id: BTreeMap<String, KvHandle> = [
            ("sessions".to_owned(), sessions),
            ("cache".to_owned(), cache),
        ]
        .into_iter()
        .collect();
        let registry: KvRegistry = StoreRegistry::new(by_id, "sessions".to_owned());

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/kv")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        assert!(ctx.kv_store("sessions").is_some());
        assert!(ctx.kv_store("cache").is_some());
        assert!(
            ctx.kv_store("unknown").is_none(),
            "registry lookups are strict: unknown ids must yield None"
        );
        assert!(ctx.kv_store_default().is_some());
    }

    #[test]
    fn kv_store_returns_none_when_only_legacy_handle_wired() {
        // Hard-cutoff: a bare `KvHandle` in extensions
        // is ignored by the registry-aware accessor. Adapter
        // dispatchers no longer insert bare handles — they
        // always synthesise a `KvRegistry` from any wired handle
        // first — so this code path only fires when a test or
        // callsite bypasses the dispatcher and inserts a bare
        // handle directly into extensions. The accessor must
        // surface that as a missing registry (None) rather than
        // silently upgrading.
        use crate::key_value_store::{KvHandle, NoopKvStore};
        use std::sync::Arc;

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/kv")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(KvHandle::new(Arc::new(NoopKvStore)));

        let ctx = RequestContext::new(request, PathParams::default());
        assert!(
            ctx.kv_store("anything").is_none(),
            "registry-aware accessor must not auto-upgrade a bare handle"
        );
        assert!(
            ctx.kv_store_default().is_none(),
            "registry-aware default accessor must not auto-upgrade a bare handle"
        );
    }

    #[test]
    fn config_store_resolves_named_handle_from_registry() {
        use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
        use crate::store_registry::{ConfigRegistry, ConfigStoreBinding, StoreRegistry};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        struct FixedStore(&'static str);
        #[async_trait(?Send)]
        impl ConfigStore for FixedStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some(self.0.to_owned()))
            }
        }

        let primary_handle = ConfigStoreHandle::new(Arc::new(FixedStore("primary")));
        let analytics_handle = ConfigStoreHandle::new(Arc::new(FixedStore("analytics")));
        let by_id: BTreeMap<String, ConfigStoreBinding> = [
            (
                "primary".to_owned(),
                ConfigStoreBinding {
                    handle: primary_handle,
                    default_key: "primary".to_owned(),
                },
            ),
            (
                "analytics".to_owned(),
                ConfigStoreBinding {
                    handle: analytics_handle,
                    default_key: "analytics".to_owned(),
                },
            ),
        ]
        .into_iter()
        .collect();
        let registry: ConfigRegistry = StoreRegistry::new(by_id, "primary".to_owned());

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        let resolved = ctx.config_store("analytics").expect("analytics handle");
        assert_eq!(
            block_on(resolved.get("key")).expect("config value"),
            Some("analytics".to_owned())
        );
        assert!(ctx.config_store("unknown").is_none());
        let default = ctx.config_store_default().expect("default handle");
        assert_eq!(
            block_on(default.get("key")).expect("default config value"),
            Some("primary".to_owned())
        );
    }

    #[test]
    fn secret_store_resolves_named_handle_from_registry() {
        use crate::secret_store::{NoopSecretStore, SecretHandle};
        use crate::store_registry::{BoundSecretStore, SecretRegistry, StoreRegistry};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let handle = SecretHandle::new(Arc::new(NoopSecretStore));
        let by_id: BTreeMap<String, BoundSecretStore> = [(
            "default".to_owned(),
            // The registry binds the logical id to the platform store name —
            // in production that's `EDGEZERO__STORES__SECRETS__DEFAULT__NAME`
            // resolved against the env (falling back to the logical id).
            BoundSecretStore::new(handle, "platform-secret-store".to_owned()),
        )]
        .into_iter()
        .collect();
        let registry: SecretRegistry = StoreRegistry::new(by_id, "default".to_owned());

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/secrets")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        let bound = ctx.secret_store("default").expect("default bound store");
        assert_eq!(bound.store_name(), "platform-secret-store");
        assert!(ctx.secret_store("unknown").is_none());
        assert!(ctx.secret_store_default().is_some());
    }

    #[test]
    fn secret_store_default_returns_none_when_only_legacy_handle_wired() {
        // Hard-cutoff: same semantics as
        // `kv_store_returns_none_when_only_legacy_handle_wired` —
        // a bare `SecretHandle` in extensions (a state that
        // only arises if a test bypasses the dispatcher) must
        // not auto-upgrade into a synthetic registry.
        use crate::secret_store::{NoopSecretStore, SecretHandle};
        use std::sync::Arc;

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/secrets")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(SecretHandle::new(Arc::new(NoopSecretStore)));

        let ctx = RequestContext::new(request, PathParams::default());
        assert!(
            ctx.secret_store_default().is_none(),
            "registry-aware default accessor must not auto-upgrade a bare handle"
        );
    }

    // -- RequestContext::config_store_default_binding / config_store_binding (B8) --

    #[test]
    fn config_store_default_binding_returns_binding_when_registry_present() {
        use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
        use crate::store_registry::{ConfigRegistry, ConfigStoreBinding, StoreRegistry};
        use std::sync::Arc;

        struct AnyStore;
        #[async_trait(?Send)]
        impl ConfigStore for AnyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(None)
            }
        }

        let binding = ConfigStoreBinding {
            handle: ConfigStoreHandle::new(Arc::new(AnyStore)),
            default_key: "resolved_key".to_owned(),
        };
        let registry: ConfigRegistry = StoreRegistry::single_id("app_config".to_owned(), binding);

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        let def = ctx.config_store_default_binding().expect("default binding");
        assert_eq!(def.default_key, "resolved_key");
    }

    #[test]
    fn config_store_default_binding_returns_none_when_no_registry() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        assert!(
            ctx.config_store_default_binding().is_none(),
            "no registry -- default binding must be None"
        );
    }

    #[test]
    fn config_store_binding_returns_named_binding_and_none_for_unknown() {
        use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
        use crate::store_registry::{ConfigRegistry, ConfigStoreBinding, StoreRegistry};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        struct AnyStore;
        #[async_trait(?Send)]
        impl ConfigStore for AnyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(None)
            }
        }

        let registry: ConfigRegistry = StoreRegistry::new(
            [
                (
                    "primary".to_owned(),
                    ConfigStoreBinding {
                        handle: ConfigStoreHandle::new(Arc::new(AnyStore)),
                        default_key: "pk".to_owned(),
                    },
                ),
                (
                    "secondary".to_owned(),
                    ConfigStoreBinding {
                        handle: ConfigStoreHandle::new(Arc::new(AnyStore)),
                        default_key: "sk".to_owned(),
                    },
                ),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            "primary".to_owned(),
        );

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());

        let sec = ctx
            .config_store_binding("secondary")
            .expect("secondary binding");
        assert_eq!(sec.default_key, "sk");

        assert!(
            ctx.config_store_binding("undeclared").is_none(),
            "unknown id must yield None"
        );
    }
}
