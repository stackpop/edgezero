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

use anyedge_core::router::RouterService;

use crate::request::into_core_request;
use crate::response::into_axum_response;

/// Tower service that adapts AnyEdge router requests to Axum/Hyper compatible responses.
#[derive(Clone)]
pub struct AnyEdgeAxumService {
    router: RouterService,
}

impl AnyEdgeAxumService {
    pub fn new(router: RouterService) -> Self {
        Self { router }
    }
}

impl Service<Request<AxumBody>> for AnyEdgeAxumService {
    type Response = Response<AxumBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<AxumBody>) -> Self::Future {
        let router = self.router.clone();
        Box::pin(async move {
            let core_request = match into_core_request(request).await {
                Ok(req) => req,
                Err(e) => {
                    let mut err_response = Response::new(Body::from(e.to_string()));
                    *err_response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;

                    return Ok(err_response);
                }
            };
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
    use anyedge_core::body::Body;
    use anyedge_core::context::RequestContext;
    use anyedge_core::error::EdgeError;
    use anyedge_core::http::{response_builder, StatusCode};
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
        let mut service = AnyEdgeAxumService::new(router);

        let request = Request::builder().uri("/").body(AxumBody::empty()).unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
