use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body as AxumBody;
use axum::http::{Request, Response};
use http::StatusCode;
use tokio::{runtime::Handle, task};
use tower::Service;

use edgezero_core::kv::KvHandle;
use edgezero_core::router::RouterService;

use crate::request::into_core_request;
use crate::response::into_axum_response;

/// Tower service that adapts EdgeZero router requests to Axum/Hyper compatible responses.
#[derive(Clone)]
pub struct EdgeZeroAxumService {
    router: RouterService,
    kv_handle: Option<KvHandle>,
}

impl EdgeZeroAxumService {
    pub fn new(router: RouterService) -> Self {
        Self {
            router,
            kv_handle: None,
        }
    }

    /// Attach a shared KV store to this service.
    ///
    /// The handle is cloned into every request's extensions, making
    /// the `Kv` extractor available in handlers.
    #[must_use]
    pub fn with_kv_handle(mut self, handle: KvHandle) -> Self {
        self.kv_handle = Some(handle);
        self
    }
}

impl Service<Request<AxumBody>> for EdgeZeroAxumService {
    type Response = Response<AxumBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<AxumBody>) -> Self::Future {
        let router = self.router.clone();
        let kv_handle = self.kv_handle.clone();
        Box::pin(async move {
            let mut core_request = match into_core_request(request).await {
                Ok(req) => req,
                Err(e) => {
                    let mut err_response = Response::new(AxumBody::from(e.to_string()));
                    *err_response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;

                    return Ok(err_response);
                }
            };

            if let Some(handle) = kv_handle {
                core_request.extensions_mut().insert(handle);
            }

            let core_response = task::block_in_place(move || {
                Handle::current().block_on(router.oneshot(core_request))
            });
            let response = into_axum_response(core_response);
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::body::Body;
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{response_builder, StatusCode};
    use tower::ServiceExt;

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
    async fn with_kv_handle_injects_into_request() {
        use crate::kv::PersistentKvStore;
        use std::sync::Arc;

        // Pre-seed the store with a value so the handler can verify injection
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let store = Arc::new(PersistentKvStore::new(db_path).unwrap());
        let handle = KvHandle::new(store.clone());
        handle.put("test_key", &"injected").await.unwrap();

        let router = RouterService::builder()
            .get("/check", |ctx: RequestContext| async move {
                let kv = ctx.kv_handle().expect("kv handle should be present");
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

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"injected");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn service_without_kv_handle_still_works() {
        let router = RouterService::builder()
            .get("/no-kv", |ctx: RequestContext| async move {
                let has_kv = ctx.kv_handle().is_some();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!("has_kv={has_kv}")))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        // No with_kv_handle call — KV is optional
        let mut service = EdgeZeroAxumService::new(router);

        let request = Request::builder()
            .uri("/no-kv")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"has_kv=false");
    }
}
