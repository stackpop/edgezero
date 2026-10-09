//! Fixed local ingress probes; no unrestricted request echo endpoint.

#[cfg(feature = "cloudflare")]
mod cloudflare;
#[cfg(feature = "spin")]
mod spin;

use async_trait::async_trait;
use edgezero_core::action;
use edgezero_core::app::Hooks;
use edgezero_core::body::Body;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Method, Request, Response, response_builder};
use edgezero_core::middleware::{Middleware, Next};
use edgezero_core::request::{CapturedTarget, Preservation, RequestIngress};
use edgezero_core::router::{PreDispatchHook, RouterService};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Store-free fixture entry hooks.
pub struct Fixture;

/// The original native client fact carried through conversion.
#[cfg(feature = "fastly")]
#[derive(Clone)]
pub struct NativeClient(pub Option<std::net::IpAddr>);

impl Hooks for Fixture {
    fn owns_logging() -> bool {
        true
    }
    fn routes() -> RouterService {
        let ordinary_calls = Arc::new(AtomicUsize::new(0));
        RouterService::builder()
            .pre_dispatch_hook(Arc::new(Probe {
                ordinary_calls: Arc::clone(&ordinary_calls),
            }))
            .middleware(CountOrdinary(ordinary_calls))
            .get("/ordinary", ordinary)
            .build()
    }
}

struct CountOrdinary(Arc<AtomicUsize>);
struct Probe {
    ordinary_calls: Arc<AtomicUsize>,
}

#[async_trait(?Send)]
impl Middleware for CountOrdinary {
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        next.run(ctx).await
    }
}

#[action]
async fn ordinary() -> &'static str {
    r#"{"ordinary":true}"#
}

#[async_trait(?Send)]
impl PreDispatchHook for Probe {
    async fn handle(&self, request: &mut Request) -> Result<Option<Response>, EdgeError> {
        let case = request
            .headers()
            .get("x-fixture")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("ready");
        if case == "continue" {
            *request.method_mut() = Method::GET;
            *request.uri_mut() = "/ordinary".parse().map_err(EdgeError::internal)?;
            return Ok(None);
        }
        let expected_target = match case {
            "dot" => "/reserved/../ordinary",
            "encoded-dot" => "/reserved/%2e%2e/ordinary",
            "slashes" => "/reserved//%2F?q=a%2Fb",
            "absolute" => "/reserved?fixed=1",
            _ => "/reserved",
        };
        let ingress = request
            .extensions()
            .get::<RequestIngress>()
            .ok_or_else(|| EdgeError::bad_request("missing ingress fixture metadata"))?;
        let (available, matches_original, matches_runtime) = match ingress.target() {
            CapturedTarget::Complete(target) => {
                let original = target.value() == expected_target
                    || target.value() == format!("http://example.com{expected_target}");
                (true, original, target.value() == *request.uri())
            }
            CapturedTarget::Unavailable(_) => (false, false, false),
        };
        let cookies: Vec<_> = request
            .headers()
            .get_all("cookie")
            .iter()
            .map(|value| value.as_bytes())
            .collect();
        let cookie_octets = cookies == [b"a=\xff".as_slice()];
        let same_name_order = cookies == [b"a=1".as_slice(), b"b=2".as_slice()];
        let cookie_name = "cookie".parse().map_err(EdgeError::internal)?;
        let fidelity = ingress.header_fidelity(&cookie_name);
        #[cfg(feature = "fastly")]
        let native_client_preserved =
            request
                .extensions()
                .get::<NativeClient>()
                .is_some_and(|native| {
                    native.0.is_some()
                        && edgezero_adapter_fastly::context::FastlyRequestContext::get(request)
                            .is_some_and(|context| context.client_ip == native.0)
                });
        #[cfg(not(feature = "fastly"))]
        let native_client_preserved = None::<bool>;
        let verdict = json!({
            "hook": true,
            "native_client_preserved": native_client_preserved,
            "ordinary": self.ordinary_calls.load(Ordering::SeqCst) != 0,
            "extension_method": request.method().as_str() == "EXAMPLE-METHOD",
            "cookie_octets": cookie_octets,
            "cookie_utf8": cookies == [b"a=\xc3\xa9".as_slice()],
            "cookie_latin1": cookies == [b"a=\xe9".as_slice()],
            "cookie_unicode": cookies == [b"a=\xc4\x80".as_slice()],
            "cookie_replacement": cookies == [b"a=\xef\xbf\xbd".as_slice()],
            "cookie_repeat_comma_joined": cookies == [b"a=1, b=2".as_slice()],
            "cookie_repeat_semicolon_joined": cookies == [b"a=1; b=2".as_slice()],
            "cookie_compound_original": cookies == [b"diagnostics=1".as_slice(), b"a=\xff".as_slice()],
            "cookie_compound_runtime_utf8": cookies == [b"diagnostics=1".as_slice(), b"a=\xef\xbf\xbd".as_slice()],
            "cookie_compound_comma_joined": cookies == [b"diagnostics=1, a=\xef\xbf\xbd".as_slice()],
            "cookie_count": cookies.len(),
            "same_name_order": same_name_order,
            "origin_count": request.headers().get_all("origin").iter().count(),
            "control_count": request.headers().get_all("x-control").iter().count(),
            "content_length_count": request.headers().get_all("content-length").iter().count(),
            "transfer_encoding_count": request.headers().get_all("transfer-encoding").iter().count(),
            "transfer_encoding_chunked": request.headers().get("transfer-encoding").is_some_and(|value| value.as_bytes() == b"chunked"),
            "target_available": available,
            "target_matches_original": matches_original,
            "target_matches_runtime": matches_runtime,
            "origin_available": ingress.origin().is_some(),
            "origin_matches_expected": ingress.origin().is_some_and(|origin| origin.scheme() == "http" && origin.authority() == "example.com"),
            "spoofed_origin_ignored": ingress.origin().is_none_or(|origin| !origin.authority().contains("spoof")),
            "global_order_unavailable": fidelity.global_field_order() == Preservation::Unavailable,
        });
        Ok(Some(
            response_builder()
                .header("content-type", "application/json")
                .body(Body::text(verdict.to_string()))
                .map_err(EdgeError::internal)?,
        ))
    }
}
