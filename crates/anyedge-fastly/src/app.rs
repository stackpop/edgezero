use anyedge_core::App;
use fastly::Request as FRequest;
use fastly::Response as FResponse;

use crate::http::{from_anyedge_response, to_anyedge_request};

/// Handle a single Fastly request with an AnyEdge `App`.
pub fn handle(app: &App, req: FRequest) -> FResponse {
    let areq = to_anyedge_request(req);
    let ares = futures::executor::block_on(app.handle(areq));
    from_anyedge_response(ares)
}
