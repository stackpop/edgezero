use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::body::Body as AxumBody;
use axum::http::{Request, Response};
use http::StatusCode;
use tokio::{runtime::Handle, task};
use tower::Service;

use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::router::RouterService;

use crate::request::into_core_request;
use crate::response::into_axum_response;

/// Tower service that adapts EdgeZero router requests to Axum/Hyper compatible responses.
#[derive(Clone)]
pub struct EdgeZeroAxumService {
    router: RouterService,
    config_store_handle: Option<ConfigStoreHandle>,
}

impl EdgeZeroAxumService {
    pub fn new(router: RouterService) -> Self {
        Self {
            router,
            config_store_handle: None,
        }
    }

    /// Attach a shared config store to this service.
    ///
    /// The handle is cloned into every request's extensions, making
    /// `ctx.config_store()` available in handlers.
    #[must_use]
    pub fn with_config_store_handle(mut self, handle: ConfigStoreHandle) -> Self {
        self.config_store_handle = Some(handle);
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
        let config_store_handle = self.config_store_handle.clone();
        Box::pin(async move {
            let mut core_request = match into_core_request(request).await {
                Ok(req) => req,
                Err(e) => {
                    let mut err_response = Response::new(Body::from(e.to_string()));
                    *err_response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;

                    return Ok(err_response);
                }
            };

            if let Some(handle) = config_store_handle {
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
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{response_builder, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    struct FixedConfigStore(String);

    impl ConfigStore for FixedConfigStore {
        fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
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
        let handle = ConfigStoreHandle::new(Arc::new(FixedConfigStore("injected".to_string())));

        let router = RouterService::builder()
            .get("/check", |ctx: RequestContext| async move {
                let store = ctx.config_store().expect("config store should be present");
                let val = store
                    .get("any_key")
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

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"injected");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn service_without_config_store_handle_still_works() {
        let router = RouterService::builder()
            .get("/no-config", |ctx: RequestContext| async move {
                let has_config = ctx.config_store().is_some();
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

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"has_config=false");
    }
}
