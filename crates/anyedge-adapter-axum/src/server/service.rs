use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body as AxumBody;
use axum::http::{Request, Response};
use tokio::{runtime::Handle, task};
use tower::Service;

use anyedge_core::router::RouterService;

use super::convert::{into_axum_response, into_core_request};

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
            let core_request = into_core_request(request);
            let core_response = task::block_in_place(move || {
                Handle::current().block_on(router.oneshot(core_request))
            });
            let response = into_axum_response(core_response);
            Ok(response)
        })
    }
}
