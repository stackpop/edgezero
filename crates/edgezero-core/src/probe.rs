//! App-owned probe routes with read-only live lifecycle observation.
//!
//! Adapters may publish a reader in request extensions. Absence does not imply
//! health or lifecycle support. Applications choose paths and middleware; these
//! helpers register no routes. Do not override framework probe/store extensions
//! through application state or middleware. Native dispatch uses a private gate.

use std::fmt;
use std::sync::Arc;

use crate::action;
use crate::body::Body;
use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::http::{Response, StatusCode, response_builder};

/// Observed native runner phase. Once draining starts, a run never becomes ready again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecyclePhase {
    Draining,
    Ready,
    Starting,
    Stopped,
}

/// Cloneable read-only observation, sampled when a handler executes.
///
/// Readiness means the selected entrypoint's startup contract succeeded, not that
/// undisclosed application requirements or ongoing dependencies were checked.
#[derive(Clone)]
pub struct LifecycleReader {
    read: Arc<dyn Fn() -> Option<LifecyclePhase> + Send + Sync>,
}

impl LifecycleReader {
    #[inline]
    #[must_use]
    pub fn new<Read>(read: Read) -> Self
    where
        Read: Fn() -> Option<LifecyclePhase> + Send + Sync + 'static,
    {
        Self {
            read: Arc::new(read),
        }
    }

    #[inline]
    #[must_use]
    pub fn phase(&self) -> Option<LifecyclePhase> {
        (self.read)()
    }
}

impl fmt::Debug for LifecycleReader {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LifecycleReader").finish_non_exhaustive()
    }
}

/// Returns 200 while starting, ready or draining, otherwise 503.
///
/// This table applies only if the handler executes. Before acceptance and after
/// listener withdrawal, supervisors must also handle refusal, reset or EOF.
///
/// # Errors
/// Returns an internal error only if the static response cannot be constructed.
#[action]
pub async fn liveness(ctx: RequestContext) -> Result<Response, EdgeError> {
    match phase(&ctx) {
        Some(LifecyclePhase::Starting | LifecyclePhase::Ready | LifecyclePhase::Draining) => {
            response(StatusCode::OK, "live")
        }
        Some(LifecyclePhase::Stopped) => response(StatusCode::SERVICE_UNAVAILABLE, "stopped"),
        None => response(StatusCode::SERVICE_UNAVAILABLE, "probe state unavailable"),
    }
}

fn phase(ctx: &RequestContext) -> Option<LifecyclePhase> {
    ctx.extensions()
        .get::<LifecycleReader>()
        .and_then(LifecycleReader::phase)
}

/// Returns 200 only in Ready, otherwise 503. Does not poll dependencies.
///
/// # Errors
/// Returns an internal error only if the static response cannot be constructed.
#[action]
pub async fn readiness(ctx: RequestContext) -> Result<Response, EdgeError> {
    match phase(&ctx) {
        Some(LifecyclePhase::Ready) => response(StatusCode::OK, "ready"),
        Some(_) => response(StatusCode::SERVICE_UNAVAILABLE, "not ready"),
        None => response(StatusCode::SERVICE_UNAVAILABLE, "probe state unavailable"),
    }
}

fn response(status: StatusCode, text: &'static str) -> Result<Response, EdgeError> {
    response_builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Body::from(text))
        .map_err(EdgeError::internal)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::executor::block_on;

    use super::*;
    use crate::http::request_builder;
    use crate::router::RouterService;

    #[test]
    fn ordinary_routes_observe_live_phase_and_missing_state() {
        #[action]
        async fn health() -> Result<&'static str, EdgeError> {
            Ok("app health")
        }
        let current = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&current);
        let reader = LifecycleReader::new(move || *observed.lock().expect("phase"));
        let router = RouterService::builder()
            .get("/app-ready", readiness)
            .get("/app-live", liveness)
            .get("/health", health)
            .build();
        for (value, ready, live) in [
            (None, 503, 503),
            (Some(LifecyclePhase::Starting), 503, 200),
            (Some(LifecyclePhase::Ready), 200, 200),
            (Some(LifecyclePhase::Draining), 503, 200),
            (Some(LifecyclePhase::Stopped), 503, 503),
        ] {
            *current.lock().expect("phase") = value;
            for (path, expected) in [("/app-ready", ready), ("/app-live", live)] {
                let mut request = request_builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request");
                request.extensions_mut().insert(reader.clone());
                let result = block_on(router.oneshot(request)).expect("probe");
                assert_eq!(result.status().as_u16(), expected);
                assert_eq!(result.headers()["cache-control"], "no-store");
                assert_eq!(
                    result.headers()["content-type"],
                    "text/plain; charset=utf-8"
                );
            }
        }
        for (path, expected) in [("/app-ready", 503), ("/health", 200), ("/ready", 404)] {
            let request = request_builder()
                .uri(path)
                .body(Body::empty())
                .expect("request");
            let result = block_on(router.oneshot(request)).expect("response");
            assert_eq!(result.status().as_u16(), expected);
        }
    }
}
