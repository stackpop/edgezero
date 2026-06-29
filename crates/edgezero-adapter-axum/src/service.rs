use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body as AxumBody;
use axum::http::{Request, Response};
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::http::StatusCode;
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::router::RouterService;
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, ConfigStoreBinding, KvRegistry, SecretRegistry,
};
use tokio::{runtime::Handle, task};
use tower::Service;

use crate::request::into_core_request;
use crate::response::into_axum_response;

/// Tower service that adapts `EdgeZero` router requests to Axum/Hyper compatible responses.
#[derive(Clone)]
pub struct EdgeZeroAxumService {
    config_registry: Option<ConfigRegistry>,
    config_store_handle: Option<ConfigStoreHandle>,
    kv_handle: Option<KvHandle>,
    kv_registry: Option<KvRegistry>,
    router: RouterService,
    secret_handle: Option<SecretHandle>,
    secret_registry: Option<SecretRegistry>,
}

impl EdgeZeroAxumService {
    #[must_use]
    #[inline]
    pub fn new(router: RouterService) -> Self {
        Self {
            config_registry: None,
            config_store_handle: None,
            kv_handle: None,
            kv_registry: None,
            router,
            secret_handle: None,
            secret_registry: None,
        }
    }

    /// Attach an id-keyed config-store registry to this service.
    #[must_use]
    #[inline]
    pub fn with_config_registry(mut self, registry: ConfigRegistry) -> Self {
        self.config_registry = Some(registry);
        self
    }

    /// Attach a shared config store to this service.
    ///
    /// Single-handle setter; the dispatcher synthesises a one-id
    /// `ConfigRegistry` keyed under `"default"`. Handlers read it
    /// via `ctx.config_store_default()` or the `Config` extractor
    /// (the pre-rewrite `ctx.config_handle()` accessor is gone --
    /// see the runtime-store-API hard-cutoff in
    /// docs/guide/manifest-store-migration.md). New code that
    /// declares multiple ids should use [`Self::with_config_registry`]
    /// directly.
    #[must_use]
    #[inline]
    pub fn with_config_store_handle(mut self, handle: ConfigStoreHandle) -> Self {
        self.config_store_handle = Some(handle);
        self
    }

    /// Attach a shared KV store to this service.
    ///
    /// Single-handle setter; the dispatcher synthesises a one-id
    /// `KvRegistry` keyed under `"default"`. Handlers read it via
    /// `ctx.kv_store_default()` or the `Kv` extractor (the
    /// pre-rewrite `ctx.kv_handle()` accessor is gone -- see the
    /// runtime-store-API hard-cutoff in
    /// docs/guide/manifest-store-migration.md). New code that
    /// declares multiple ids should use [`Self::with_kv_registry`]
    /// directly.
    #[must_use]
    #[inline]
    pub fn with_kv_handle(mut self, handle: KvHandle) -> Self {
        self.kv_handle = Some(handle);
        self
    }

    /// Attach an id-keyed KV registry to this service.
    #[must_use]
    #[inline]
    pub fn with_kv_registry(mut self, registry: KvRegistry) -> Self {
        self.kv_registry = Some(registry);
        self
    }

    /// Attach a shared secret store to this service.
    ///
    /// Single-handle setter; the dispatcher synthesises a one-id
    /// `SecretRegistry` keyed under `"default"` (the handle is
    /// bound to the platform store name `"default"`). Handlers
    /// read it via `ctx.secret_store_default()` or the `Secrets`
    /// extractor (the pre-rewrite `ctx.secret_handle()` accessor
    /// is gone -- see the runtime-store-API hard-cutoff in
    /// docs/guide/manifest-store-migration.md). New code that
    /// declares multiple ids should use
    /// [`Self::with_secret_registry`] directly.
    #[must_use]
    #[inline]
    pub fn with_secret_handle(mut self, handle: SecretHandle) -> Self {
        self.secret_handle = Some(handle);
        self
    }

    /// Attach an id-keyed secret-store registry to this service.
    #[must_use]
    #[inline]
    pub fn with_secret_registry(mut self, registry: SecretRegistry) -> Self {
        self.secret_registry = Some(registry);
        self
    }
}

impl Service<Request<AxumBody>> for EdgeZeroAxumService {
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    type Response = Response<AxumBody>;

    #[inline]
    fn call(&mut self, req: Request<AxumBody>) -> Self::Future {
        let router = self.router.clone();
        // Hard-cutoff: legacy bare `KvHandle` /
        // `ConfigStoreHandle` / `SecretHandle` entries are NO
        // LONGER inserted into request extensions. The legacy
        // `with_*_handle` constructors still take a single
        // handle, but the dispatcher synthesises a one-id
        // `<kind>Registry` under the conventional `"default"`
        // id from that handle — and only the registry goes into
        // extensions. Handlers must use the registry-aware
        // `RequestContext` accessors (`kv_store_default`,
        // `config_store_default`, `secret_store_default`) or
        // the `Kv` / `Config` / `Secrets` extractors. The
        // pre-rewrite `ctx.kv_handle()` / `config_handle()` /
        // `secret_handle()` accessors are gone (spec
        // hard-cutoff).
        let config_registry = self.config_registry.clone().or_else(|| {
            self.config_store_handle.clone().map(|handle| {
                ConfigRegistry::single_id(
                    "default".to_owned(),
                    ConfigStoreBinding {
                        handle,
                        default_key: "default".to_owned(),
                    },
                )
            })
        });
        let kv_registry = self.kv_registry.clone().or_else(|| {
            self.kv_handle
                .clone()
                .map(|handle| KvRegistry::single_id("default".to_owned(), handle))
        });
        let secret_registry = self.secret_registry.clone().or_else(|| {
            self.secret_handle.clone().map(|handle| {
                SecretRegistry::single_id(
                    "default".to_owned(),
                    BoundSecretStore::new(handle, "default".to_owned()),
                )
            })
        });
        Box::pin(async move {
            let mut core_request = match into_core_request(req).await {
                Ok(converted) => converted,
                Err(err) => {
                    let mut err_response = Response::new(AxumBody::from(err.clone()));
                    *err_response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;

                    return Ok(err_response);
                }
            };

            if let Some(registry) = config_registry {
                core_request.extensions_mut().insert(registry);
            }
            if let Some(registry) = kv_registry {
                core_request.extensions_mut().insert(registry);
            }
            if let Some(registry) = secret_registry {
                core_request.extensions_mut().insert(registry);
            }

            let core_response = task::block_in_place(move || {
                Handle::current().block_on(router.oneshot(core_request))
            });
            let response = match core_response {
                Ok(response) => into_axum_response(response),
                Err(err) => {
                    let body = AxumBody::from(format!("internal error: {err}"));
                    let mut fallback = Response::new(body);
                    *fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                    fallback
                }
            };
            Ok(response)
        })
    }

    #[inline]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use edgezero_core::body::Body;
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{response_builder, StatusCode};
    use edgezero_core::key_value_store::KvStore;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    struct FixedConfigStore(String);

    #[async_trait::async_trait(?Send)]
    impl ConfigStore for FixedConfigStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(Some(self.0.clone()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_request_to_router() {
        let router = RouterService::builder()
            .get("/", |_ctx: RequestContext| async move {
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from("ok"))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = EdgeZeroAxumService::new(router);

        let request = Request::builder().uri("/").body(AxumBody::empty()).unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_config_store_handle_injects_into_request() {
        // Hard-cutoff: legacy `ctx.config_handle()` is
        // gone. The service synthesises a one-id `ConfigRegistry`
        // from the wired handle at the dispatch boundary, so
        // `ctx.config_store_default()` resolves the same store.
        let handle = ConfigStoreHandle::new(Arc::new(FixedConfigStore("injected".to_owned())));

        let router = RouterService::builder()
            .get("/check", |ctx: RequestContext| async move {
                let store = ctx
                    .config_store_default()
                    .expect("config store should be present");
                let val = store
                    .get("any_key")
                    .await
                    .expect("config lookup should succeed")
                    .unwrap_or_default();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(val))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = EdgeZeroAxumService::new(router).with_config_store_handle(handle);

        let request = Request::builder()
            .uri("/check")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&*body, b"injected");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_kv_handle_injects_into_request() {
        use crate::key_value_store::PersistentKvStore;

        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let store: Arc<dyn KvStore> = Arc::new(PersistentKvStore::new(db_path).unwrap());
        let handle = KvHandle::new(Arc::clone(&store));
        handle.put("test_key", &"injected").await.unwrap();

        let router = RouterService::builder()
            .get("/check", |ctx: RequestContext| async move {
                // Hard-cutoff: see
                // `with_config_store_handle_injects_into_request`.
                let kv = ctx.kv_store_default().expect("kv handle should be present");
                let val: String = kv.get_or("test_key", String::new()).await.unwrap();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(val))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = EdgeZeroAxumService::new(router).with_kv_handle(handle);

        let request = Request::builder()
            .uri("/check")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&*body, b"injected");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kv_registry_wins_over_bare_handle_when_both_wired() {
        // Documents the precedence rule baked into the dispatcher:
        // `self.kv_registry.clone().or_else(|| self.kv_handle.map(...single_id))`.
        // If a caller wires BOTH `.with_kv_registry(...)` and
        // `.with_kv_handle(...)`, the registry wins outright -- the
        // bare handle is NOT used as a fallback for ids the registry
        // doesn't define, and is NOT synthesised into a "default"
        // entry alongside the registry's ids.
        use crate::key_value_store::PersistentKvStore;
        use edgezero_core::store_registry::{KvRegistry, StoreRegistry};
        use std::collections::BTreeMap;

        let temp_dir = tempfile::tempdir().unwrap();
        let registry_store: Arc<dyn KvStore> =
            Arc::new(PersistentKvStore::new(temp_dir.path().join("registry.redb")).unwrap());
        let registry_handle = KvHandle::new(Arc::clone(&registry_store));
        registry_handle
            .put("marker", &"from_registry")
            .await
            .unwrap();

        let handle_store: Arc<dyn KvStore> =
            Arc::new(PersistentKvStore::new(temp_dir.path().join("handle.redb")).unwrap());
        let bare_handle = KvHandle::new(Arc::clone(&handle_store));
        bare_handle.put("marker", &"from_bare").await.unwrap();

        // Registry binds only `sessions` (NOT `default`). If the
        // dispatcher merged in the bare handle, `default` would
        // resolve to the bare-handle store; the test asserts it does
        // NOT.
        let by_id: BTreeMap<String, KvHandle> = [("sessions".to_owned(), registry_handle)]
            .into_iter()
            .collect();
        let registry: KvRegistry = StoreRegistry::new(by_id, "sessions".to_owned());

        let router = RouterService::builder()
            .get("/probe", |ctx: RequestContext| async move {
                // Registry's id resolves to the registry's store.
                let named = ctx.kv_store("sessions").expect("registry binding");
                let from_named: String = named.get_or("marker", String::new()).await.unwrap();
                // Default ALSO resolves to the registry (registry's
                // own declared default), NOT the bare handle.
                let default = ctx.kv_store_default().expect("registry default");
                let from_default: String = default.get_or("marker", String::new()).await.unwrap();
                // The bare handle's synthesised `default` id is NOT
                // exposed -- registry wins outright.
                let bare_default_visible = ctx.kv_store("default").is_some();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!(
                        "named={from_named} default={from_default} bare_default={bare_default_visible}"
                    )))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        // Wire BOTH: registry first, then a bare handle. The bare
        // handle would synthesise a "default" id under the legacy
        // path; the dispatcher's `or_else` precedence must skip it.
        let mut service = EdgeZeroAxumService::new(router)
            .with_kv_registry(registry)
            .with_kv_handle(bare_handle);

        let request = Request::builder()
            .uri("/probe")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            &*body, b"named=from_registry default=from_registry bare_default=false",
            "registry must win: bare handle is neither merged in nor a fallback"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_kv_handle_synthesises_one_id_registry_under_default() {
        // Verifies the one-id-registry contract for the setup API:
        // `with_kv_handle(h)` wraps `h` in a `KvRegistry` with the
        // logical id `"default"`. So in a handler:
        //   - `ctx.kv_store_default()` must resolve.
        //   - `ctx.kv_store("default")` must resolve to the same handle.
        //   - `ctx.kv_store("any-other-id")` must return None (the
        //     registry has only one id; named lookups for anything
        //     else are misses, not silent fallbacks).
        // This is the precedence guarantee that lets handlers use
        // the named-lookup path uniformly across adapters with one
        // or many declared stores.
        use crate::key_value_store::PersistentKvStore;

        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let store: Arc<dyn KvStore> = Arc::new(PersistentKvStore::new(db_path).unwrap());
        let handle = KvHandle::new(Arc::clone(&store));
        handle.put("k", &"v").await.unwrap();

        let router = RouterService::builder()
            .get("/probe", |ctx: RequestContext| async move {
                let by_default = ctx.kv_store_default().is_some();
                let by_default_name = ctx.kv_store("default").is_some();
                let unknown = ctx.kv_store("custom-id").is_none();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!(
                        "default={by_default} named_default={by_default_name} unknown_is_none={unknown}"
                    )))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = EdgeZeroAxumService::new(router).with_kv_handle(handle);

        let request = Request::builder()
            .uri("/probe")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            &*body, b"default=true named_default=true unknown_is_none=true",
            "synthesised one-id registry: default + named-`default` resolve; unknown id misses"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn service_without_config_store_handle_still_works() {
        let router = RouterService::builder()
            .get("/no-config", |ctx: RequestContext| async move {
                // Hard-cutoff: with no handle and no
                // registry wired, the registry-aware accessor
                // returns None — same observable result as the
                // legacy `config_handle().is_some()` check.
                let has_config = ctx.config_store_default().is_some();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!("has_config={has_config}")))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = EdgeZeroAxumService::new(router);

        let request = Request::builder()
            .uri("/no-config")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&*body, b"has_config=false");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_secret_handle_injects_into_request() {
        use bytes::Bytes;
        use edgezero_core::secret_store::{InMemorySecretStore, SecretHandle};
        use std::sync::Arc;

        // Hard-cutoff: the service synthesises a one-id
        // `SecretRegistry` from `with_secret_handle`, binding the
        // handle under the platform store name `"default"`. The
        // fixture keys mirror that bound name (`"default/<key>"`)
        // so the registry-aware lookup resolves.
        let handle = SecretHandle::new(Arc::new(InMemorySecretStore::new([(
            "default/__EDGEZERO_SERVICE_TEST_SECRET__",
            Bytes::from("injected_value"),
        )])));
        let router = RouterService::builder()
            .get("/check", |ctx: RequestContext| async move {
                // `BoundSecretStore::get_bytes(key)` is single-arg —
                // the platform store name is bound by the
                // dispatcher's synthesis.
                let secrets = ctx
                    .secret_store_default()
                    .expect("secret store should be present");
                let val = secrets
                    .get_bytes("__EDGEZERO_SERVICE_TEST_SECRET__")
                    .await
                    .unwrap()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(val))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = EdgeZeroAxumService::new(router).with_secret_handle(handle);

        let request = Request::builder()
            .uri("/check")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&*body, b"injected_value");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn service_without_kv_handle_still_works() {
        let router = RouterService::builder()
            .get("/no-kv", |ctx: RequestContext| async move {
                // Hard-cutoff: see
                // `service_without_config_store_handle_still_works`.
                let has_kv = ctx.kv_store_default().is_some();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!("has_kv={has_kv}")))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = EdgeZeroAxumService::new(router);

        let request = Request::builder()
            .uri("/no-kv")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&*body, b"has_kv=false");
    }

    /// Two-id KV registry: `ctx.kv_store("sessions")` and
    /// `ctx.kv_store("cache")` must each resolve to their own backing store.
    /// `ctx.kv_store_default()` must resolve to the registered default id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_kv_registry_resolves_named_and_default() {
        use crate::key_value_store::PersistentKvStore;
        use edgezero_core::store_registry::{KvRegistry, StoreRegistry};
        use std::collections::BTreeMap;

        let temp_dir = tempfile::tempdir().unwrap();

        let sessions_store: Arc<dyn KvStore> =
            Arc::new(PersistentKvStore::new(temp_dir.path().join("sessions.redb")).unwrap());
        let sessions_handle = KvHandle::new(Arc::clone(&sessions_store));
        sessions_handle
            .put("greeting", &"hello-from-sessions")
            .await
            .unwrap();

        let cache_store: Arc<dyn KvStore> =
            Arc::new(PersistentKvStore::new(temp_dir.path().join("cache.redb")).unwrap());
        let cache_handle = KvHandle::new(Arc::clone(&cache_store));
        cache_handle
            .put("greeting", &"hello-from-cache")
            .await
            .unwrap();

        let by_id: BTreeMap<String, KvHandle> = [
            ("sessions".to_owned(), sessions_handle),
            ("cache".to_owned(), cache_handle),
        ]
        .into_iter()
        .collect();
        let registry: KvRegistry = StoreRegistry::new(by_id, "sessions".to_owned());

        let router = RouterService::builder()
            .get("/named/{id}", |ctx: RequestContext| async move {
                let id = ctx
                    .path_params()
                    .get("id")
                    .map(ToOwned::to_owned)
                    .unwrap_or_default();
                let store = ctx
                    .kv_store(&id)
                    .ok_or_else(|| EdgeError::not_found(format!("kv id `{id}` not registered")))?;
                let value: String = store.get_or("greeting", String::new()).await.unwrap();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(value))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .get("/default", |ctx: RequestContext| async move {
                let store = ctx
                    .kv_store_default()
                    .expect("default kv store is registered");
                let value: String = store.get_or("greeting", String::new()).await.unwrap();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(value))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let service = EdgeZeroAxumService::new(router).with_kv_registry(registry);

        assert_eq!(
            body_at(&service, "/named/sessions").await,
            "hello-from-sessions"
        );
        assert_eq!(body_at(&service, "/named/cache").await, "hello-from-cache");
        assert_eq!(body_at(&service, "/default").await, "hello-from-sessions");
    }

    /// Unknown ids on a wired registry yield `None` — strict lookup, no
    /// fallback to the default. The handler returns 404 in that case.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kv_registry_lookup_is_strict_for_unknown_ids() {
        use crate::key_value_store::PersistentKvStore;
        use edgezero_core::store_registry::{KvRegistry, StoreRegistry};
        use std::collections::BTreeMap;

        let temp_dir = tempfile::tempdir().unwrap();
        let only_store: Arc<dyn KvStore> =
            Arc::new(PersistentKvStore::new(temp_dir.path().join("only.redb")).unwrap());
        let only_handle = KvHandle::new(Arc::clone(&only_store));

        let by_id: BTreeMap<String, KvHandle> =
            [("only".to_owned(), only_handle)].into_iter().collect();
        let registry: KvRegistry = StoreRegistry::new(by_id, "only".to_owned());

        let router = RouterService::builder()
            .get("/lookup/{id}", |ctx: RequestContext| async move {
                let id = ctx
                    .path_params()
                    .get("id")
                    .map(ToOwned::to_owned)
                    .unwrap_or_default();
                let present = ctx.kv_store(&id).is_some();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!("present={present}")))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let service = EdgeZeroAxumService::new(router).with_kv_registry(registry);

        assert_eq!(body_at(&service, "/lookup/only").await, "present=true");
        assert_eq!(body_at(&service, "/lookup/missing").await, "present=false");
    }

    /// Send a GET request through `service` and return the response body as a UTF-8 string.
    /// Lifted out of the registry-aware tests so each can stay flat (clippy
    /// `items_after_statements` rejects nested `async fn` definitions).
    async fn body_at(service: &EdgeZeroAxumService, path: &str) -> String {
        let request = Request::builder()
            .uri(path)
            .body(AxumBody::empty())
            .unwrap();
        let mut svc = service.clone();
        let response = svc.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }
}
