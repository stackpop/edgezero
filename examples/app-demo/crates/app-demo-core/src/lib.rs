pub mod config;
// `handlers` is `pub` so downstream integration tests
// can dispatch them directly against a wired `ConfigRegistry` /
// `KvRegistry` / `SecretRegistry` — the same fixture shape the
// runtime sets up. This avoids spinning a real HTTP server in
// tests that only need to verify the push → read-back → handler
// contract end to end. The `app!` macro still uses the handlers
// internally; pub visibility is purely additive.
pub mod handlers;

use std::error::Error;
use std::fmt;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use edgezero_core::app::App as EdgeZeroApp;
use edgezero_core::http::StatusCode;
use edgezero_core::{
    AdmissionDecision, BufferedIngressResponse, ConfigExtractionLimits,
    DetachedResponseEgressDecision, EdgeError, IngressGrant, MemoryCeiling, MemoryCeilingScope,
    MemoryEnvelope, MemoryEnvelopeValidation, MemoryEnvelopeValidationError, MemoryResource,
    PlatformMetadata, ResponseEgressCompletion, ResponseEgressResource,
    ResponseEgressResourceInstallError, RouteResolution, DEFAULT_RESPONSE_WRITE_BUDGET,
};

const DEFAULT_INGRESS_READ_BUDGET: Duration = Duration::from_secs(30);
const FALLBACK_INGRESS_BODY_BYTES: usize = 4 * 1024;
const FALLBACK_INGRESS_READ_BUDGET: Duration = Duration::from_secs(5);
const OUTBOUND_INGRESS_READ_BUDGET: Duration = Duration::from_secs(10);
const EXPECTED_FIXED_MEMORY_BYTES: u64 = 4 * 1024 * 1024;
const EXPECTED_PER_LIVE_REQUEST_BYTES: u64 = 1024 * 1024;
const EXPECTED_SEPARATE_STACK_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlatformMemoryStartupError {
    Exceeds {
        available_bytes: u64,
        required_bytes: u64,
        resource: MemoryResource,
    },
    UnsupportedValidationOutcome,
    Validation {
        source: MemoryEnvelopeValidationError,
    },
}

impl fmt::Display for PlatformMemoryStartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Exceeds {
                available_bytes,
                required_bytes,
                resource,
            } => {
                let resource_label = match resource {
                    MemoryResource::Primary => "primary memory",
                    MemoryResource::SeparateStack => "separate stack",
                    _ => "unrecognized memory resource",
                };
                write!(
                    f,
                    "application memory envelope exceeds {resource_label}: required {required_bytes} bytes, available {available_bytes} bytes"
                )
            }
            Self::UnsupportedValidationOutcome => {
                f.write_str("unsupported platform memory validation outcome")
            }
            Self::Validation { source } => source.fmt(f),
        }
    }
}

#[expect(
    clippy::missing_trait_methods,
    reason = "the typed validation source is the only nested startup error"
)]
impl Error for PlatformMemoryStartupError {
    #[expect(
        clippy::pattern_type_mismatch,
        reason = "match ergonomics preserve the borrowed validation source"
    )]
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Validation { source } => Some(source),
            Self::Exceeds { .. } | Self::UnsupportedValidationOutcome => None,
        }
    }
}

#[derive(Debug)]
struct AdmissionLease {
    #[cfg(test)]
    release_counter: Option<Arc<AtomicUsize>>,
    response_resource: ResponseEgressResource<ResponsePermit>,
    route_class: Option<String>,
}

impl AdmissionLease {
    fn install_response_permit(&self) -> Result<(), ResponseEgressResourceInstallError> {
        let permit = ResponsePermit {
            #[cfg(test)]
            release_counter: self.release_counter.clone(),
            route_class: self.route_class.clone(),
        };
        self.response_resource.install(permit)
    }

    #[cfg(test)]
    fn observe_response_permit_releases(&mut self, counter: Arc<AtomicUsize>) {
        self.release_counter = Some(counter);
    }
}

/// Demonstration stand-in for an application semaphore permit.
#[derive(Debug)]
struct ResponsePermit {
    #[cfg(test)]
    release_counter: Option<Arc<AtomicUsize>>,
    route_class: Option<String>,
}

impl ResponsePermit {
    fn new(route_class: Option<String>) -> Self {
        Self {
            #[cfg(test)]
            release_counter: None,
            route_class,
        }
    }

    fn release(self) {
        drop(self);
    }
}

impl Drop for ResponsePermit {
    fn drop(&mut self) {
        drop(self.route_class.take());
        #[cfg(test)]
        if let Some(counter) = self.release_counter.take() {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// App-owned shared state for the `app!(..., state = ...)` demonstration,
/// handed to handlers via `State<Arc<DemoState>>`.
#[derive(Debug)]
pub struct DemoState {
    /// A greeting the handler echoes, proving the value reached the handler.
    pub greeting: String,
}

/// Installs request-lifecycle policy before any adapter begins polling a body.
fn configure_app(app: &mut EdgeZeroApp) -> Result<(), EdgeError> {
    platform_memory_startup(app.platform()).map_err(EdgeError::internal)?;
    app.set_config_extraction_limits(ConfigExtractionLimits::default())?;
    app.set_detached_response_egress_decision_factory(|head| {
        let permit = ResponsePermit::new(Some(format!("detached:{}", head.error_kind())));
        let completion = ResponseEgressCompletion::new(move |_report| {
            permit.release();
        });
        DetachedResponseEgressDecision::Send {
            completion,
            deadline: head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET),
        }
    });
    app.set_ingress_admission_policy(|head| {
        if let RouteResolution::Matched(metadata) = head.route_resolution().clone() {
            let route_class = metadata.class().map(str::to_owned);
            let read_budget = if route_class.as_deref() == Some("outbound") {
                OUTBOUND_INGRESS_READ_BUDGET
            } else {
                DEFAULT_INGRESS_READ_BUDGET
            };
            let permit = ResponsePermit::new(route_class.clone());
            let (lease, completion) = response_lifecycle(route_class, permit);
            AdmissionDecision::Admit {
                completion,
                grant: IngressGrant::new(lease),
                read_deadline: head.read_deadline_after(read_budget),
            }
        } else {
            let (lease, completion) = response_lifecycle(None, ResponsePermit::new(None));
            AdmissionDecision::ReadBodyBeforeFallback {
                completion,
                grant: IngressGrant::new(lease),
                max_body_bytes: FALLBACK_INGRESS_BODY_BYTES,
                read_deadline: head.read_deadline_after(FALLBACK_INGRESS_READ_BUDGET),
                on_exceeded: BufferedIngressResponse::text(
                    StatusCode::BAD_REQUEST,
                    "request body too large\n",
                ),
                on_timeout: BufferedIngressResponse::text(
                    StatusCode::REQUEST_TIMEOUT,
                    "request timeout\n",
                ),
            }
        }
    });
    Ok(())
}

fn platform_memory_validation(
    platform: PlatformMetadata,
) -> Result<MemoryEnvelopeValidation, MemoryEnvelopeValidationError> {
    let known_ceiling = platform.memory_ceiling().value();
    let scope = known_ceiling.map_or(MemoryCeilingScope::PerExecution, MemoryCeiling::scope);
    let (fixed_bytes, separate_stack_bytes) =
        match known_ceiling.and_then(MemoryCeiling::stack_bytes) {
            Some(_) => (
                EXPECTED_FIXED_MEMORY_BYTES,
                Some(EXPECTED_SEPARATE_STACK_BYTES),
            ),
            None => (
                EXPECTED_FIXED_MEMORY_BYTES + EXPECTED_SEPARATE_STACK_BYTES,
                None,
            ),
        };
    let envelope = MemoryEnvelope::new(
        scope,
        fixed_bytes,
        EXPECTED_PER_LIVE_REQUEST_BYTES,
        separate_stack_bytes,
    );
    platform.validate_memory_envelope(envelope)
}

fn platform_memory_startup(platform: PlatformMetadata) -> Result<(), PlatformMemoryStartupError> {
    let validation = platform_memory_validation(platform)
        .map_err(|source| PlatformMemoryStartupError::Validation { source })?;
    match validation {
        MemoryEnvelopeValidation::Fits { .. } => Ok(()),
        MemoryEnvelopeValidation::Indeterminate { .. } => {
            // Unknown facts remain indeterminate; startup may continue without claiming `Fits`
            // or adding an application concurrency gate.
            Ok(())
        }
        MemoryEnvelopeValidation::Exceeds {
            available_bytes,
            required_bytes,
            resource,
        } => Err(PlatformMemoryStartupError::Exceeds {
            available_bytes,
            required_bytes,
            resource,
        }),
        _ => Err(PlatformMemoryStartupError::UnsupportedValidationOutcome),
    }
}

/// Composes a held permit with a core-owned late-bound response resource.
fn response_lifecycle(
    route_class: Option<String>,
    permit: ResponsePermit,
) -> (AdmissionLease, ResponseEgressCompletion) {
    let static_completion = ResponseEgressCompletion::new(move |_report| {
        permit.release();
    });
    let (response_resource, late_bound_completion) = ResponseEgressCompletion::late_bound();
    (
        AdmissionLease {
            #[cfg(test)]
            release_counter: None,
            response_resource,
            route_class,
        },
        static_completion.join(late_bound_completion),
    )
}

/// Returns the shared app state, referenced by `app!(..., state = crate::app_state())`.
///
/// IMPORTANT: `app!(state = <expr>)` emits this call inside the macro-generated
/// `build_router()`, which every adapter's `run_app` invokes via
/// `EdgeZeroApp::build::<A>(platform)`
/// — once at startup for long-lived runtimes (Axum), but **once per request** on
/// default Fastly, Cloudflare and Spin entry points. Fastly starts a fresh Wasm
/// instance by default; Cloudflare and Spin also rebuild the app per request.
/// So `app_state()` must be **cheap**: build the heavy state once and hand out
/// clones. Here a `OnceLock<Arc<DemoState>>` builds it lazily and every call just
/// bumps the `Arc` refcount — do NOT `Arc::new(..)` a heavy object on each call.
#[must_use]
#[inline]
pub fn app_state() -> Arc<DemoState> {
    static STATE: OnceLock<Arc<DemoState>> = OnceLock::new();
    Arc::clone(STATE.get_or_init(|| {
        Arc::new(DemoState {
            greeting: "hello from app state".to_owned(),
        })
    }))
}

edgezero_core::app!(
    "../../edgezero.toml",
    configure = crate::configure_app,
    state = crate::app_state()
);

#[cfg(test)]
mod lifecycle_tests {
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use edgezero_core::app::App as EdgeZeroApp;
    use edgezero_core::body::Body;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{request_builder, HeaderMap, Method, Response, StatusCode, Version};
    use edgezero_core::ingress::{
        AdmissionDecision, IngressBeginOutcome, IngressDispatchOutcome, IngressGrant,
        IngressHeadParts,
    };
    use edgezero_core::response_egress::ResponseEgressEnvelope;
    use edgezero_core::router::RouteResolution;
    use edgezero_core::time::MonotonicInstant;
    use futures::executor::block_on;
    use futures::stream::iter;

    #[test]
    fn manifest_route_classes_reach_route_resolution() {
        let resolved = crate::build_router().resolve(&Method::GET, "/proxy/status/200");
        let RouteResolution::Matched(metadata) = resolved.resolution().clone() else {
            panic!("proxy route must resolve");
        };
        assert_eq!(metadata.class(), Some("outbound"));
    }

    #[test]
    fn platform_memory_validation_preserves_unknown_facts() {
        let validation =
            super::platform_memory_validation(edgezero_core::PlatformMetadata::default())
                .expect("valid demo envelope");

        let edgezero_core::MemoryEnvelopeValidation::Indeterminate { reasons, .. } = validation
        else {
            panic!("unknown platform facts must prevent complete validation");
        };
        assert_eq!(reasons.len(), 3);
    }

    fn known_platform(
        ceiling: edgezero_core::MemoryCeiling,
        population: NonZeroU32,
    ) -> edgezero_core::PlatformMetadata {
        let source = edgezero_core::PlatformResourceSource::PlatformLimit {
            provider: "test platform",
        };
        edgezero_core::PlatformMetadata::new(
            edgezero_core::PlatformFact::known(ceiling, source),
            edgezero_core::PlatformFact::known(
                edgezero_core::InboundRequestPopulationBound::new(population),
                source,
            ),
            edgezero_core::PlatformFact::known(
                edgezero_core::HostIngressMemoryAccounting::OutsideCeiling,
                source,
            ),
        )
    }

    #[test]
    fn platform_memory_validation_accounts_for_both_stack_shapes() {
        let primary_without_separate_stack = super::EXPECTED_FIXED_MEMORY_BYTES
            + super::EXPECTED_SEPARATE_STACK_BYTES
            + super::EXPECTED_PER_LIVE_REQUEST_BYTES;
        let without_separate_stack = super::platform_memory_validation(known_platform(
            edgezero_core::MemoryCeiling::new(
                primary_without_separate_stack,
                edgezero_core::MemoryCeilingScope::PerExecution,
                None,
            ),
            NonZeroU32::MIN,
        ))
        .expect("valid envelope without separate stack");
        assert!(matches!(
            without_separate_stack,
            edgezero_core::MemoryEnvelopeValidation::Fits {
                required_primary_bytes,
                required_separate_stack_bytes: None,
                ..
            } if required_primary_bytes == primary_without_separate_stack
        ));

        let primary_with_separate_stack =
            super::EXPECTED_FIXED_MEMORY_BYTES + super::EXPECTED_PER_LIVE_REQUEST_BYTES;
        let with_separate_stack = super::platform_memory_validation(known_platform(
            edgezero_core::MemoryCeiling::new(
                primary_with_separate_stack,
                edgezero_core::MemoryCeilingScope::PerExecution,
                Some(super::EXPECTED_SEPARATE_STACK_BYTES),
            ),
            NonZeroU32::MIN,
        ))
        .expect("valid envelope with separate stack");
        assert!(matches!(
            with_separate_stack,
            edgezero_core::MemoryEnvelopeValidation::Fits {
                required_primary_bytes,
                required_separate_stack_bytes: Some(required_stack_bytes),
                ..
            } if required_primary_bytes == primary_with_separate_stack
                && required_stack_bytes == super::EXPECTED_SEPARATE_STACK_BYTES
        ));
    }

    #[test]
    fn platform_memory_startup_accepts_fit() {
        let required_primary =
            super::EXPECTED_FIXED_MEMORY_BYTES + super::EXPECTED_PER_LIVE_REQUEST_BYTES;
        let platform = known_platform(
            edgezero_core::MemoryCeiling::new(
                required_primary,
                edgezero_core::MemoryCeilingScope::PerExecution,
                Some(super::EXPECTED_SEPARATE_STACK_BYTES),
            ),
            NonZeroU32::MIN,
        );

        assert_eq!(super::platform_memory_startup(platform), Ok(()));
    }

    #[test]
    fn platform_memory_startup_preserves_primary_excess() {
        let platform = known_platform(
            edgezero_core::MemoryCeiling::new(
                1,
                edgezero_core::MemoryCeilingScope::PerExecution,
                None,
            ),
            NonZeroU32::MIN,
        );

        assert!(matches!(
            super::platform_memory_startup(platform),
            Err(super::PlatformMemoryStartupError::Exceeds {
                available_bytes: 1,
                required_bytes,
                resource: edgezero_core::MemoryResource::Primary,
            }) if required_bytes == super::EXPECTED_FIXED_MEMORY_BYTES
                + super::EXPECTED_SEPARATE_STACK_BYTES
                + super::EXPECTED_PER_LIVE_REQUEST_BYTES
        ));
    }

    #[test]
    fn platform_memory_startup_preserves_stack_excess() {
        let platform = known_platform(
            edgezero_core::MemoryCeiling::new(
                super::EXPECTED_FIXED_MEMORY_BYTES + super::EXPECTED_PER_LIVE_REQUEST_BYTES,
                edgezero_core::MemoryCeilingScope::PerExecution,
                Some(1),
            ),
            NonZeroU32::MIN,
        );

        assert_eq!(
            super::platform_memory_startup(platform),
            Err(super::PlatformMemoryStartupError::Exceeds {
                available_bytes: 1,
                required_bytes: super::EXPECTED_SEPARATE_STACK_BYTES,
                resource: edgezero_core::MemoryResource::SeparateStack,
            })
        );
    }

    #[test]
    fn platform_memory_startup_keeps_indeterminate_distinct_from_fit() {
        assert_eq!(
            super::platform_memory_startup(edgezero_core::PlatformMetadata::default()),
            Ok(())
        );
    }

    #[test]
    fn platform_memory_startup_preserves_validation_error() {
        let platform = known_platform(
            edgezero_core::MemoryCeiling::new(
                u64::MAX,
                edgezero_core::MemoryCeilingScope::PerExecution,
                Some(super::EXPECTED_SEPARATE_STACK_BYTES),
            ),
            NonZeroU32::new(2).expect("non-zero population"),
        );

        assert!(matches!(
            super::platform_memory_startup(platform),
            Err(super::PlatformMemoryStartupError::Validation {
                source:
                    edgezero_core::MemoryEnvelopeValidationError::NonUnitPerExecutionPopulation {
                        max_live_requests,
                    },
            }) if max_live_requests.get() == 2
        ));
    }

    #[test]
    fn configured_admission_uses_finite_class_aware_deadlines() {
        let app = EdgeZeroApp::build::<super::App>(edgezero_core::PlatformMetadata::default())
            .expect("configured app");
        let start = MonotonicInstant::now();

        let outbound = begin_ingress(&app, "/proxy/status/200", start);
        assert_eq!(
            outbound.read_deadline().instant(),
            start
                .checked_add(Duration::from_secs(10))
                .expect("deadline")
        );

        let health = begin_ingress(&app, "/", start);
        assert_eq!(
            health.read_deadline().instant(),
            start
                .checked_add(Duration::from_secs(30))
                .expect("deadline")
        );

        let fallback = begin_ingress(&app, "/missing", start);
        assert_eq!(
            fallback.read_deadline().instant(),
            start.checked_add(Duration::from_secs(5)).expect("deadline")
        );
    }

    #[test]
    fn admission_handler_consumes_the_typed_grant_once() {
        let app = EdgeZeroApp::build::<super::App>(edgezero_core::PlatformMetadata::default())
            .expect("configured app");
        let start = MonotonicInstant::now();
        let prepared = begin_ingress(&app, "/admission", start);
        let request = request_builder()
            .method(Method::GET)
            .uri("/admission")
            .body(Body::empty())
            .expect("request");

        let response = complete_envelope(block_on(app.dispatch_admitted(prepared, request)));
        let payload: serde_json::Value = response.body().to_json().expect("json");
        assert_eq!(payload["route_class"], "diagnostic");
        assert_eq!(payload["grant_consumed_once"], true);
    }

    #[test]
    fn late_bound_permit_lives_until_terminal_egress() {
        let releases = Arc::new(AtomicUsize::new(0));
        let policy_releases = Arc::clone(&releases);
        let mut app = EdgeZeroApp::build::<super::App>(edgezero_core::PlatformMetadata::default())
            .expect("configured app");
        app.set_ingress_admission_policy(move |head| {
            let RouteResolution::Matched(metadata) = head.route_resolution().clone() else {
                panic!("admission route must resolve");
            };
            let route_class = metadata.class().map(str::to_owned);
            let permit = super::ResponsePermit::new(route_class.clone());
            let (mut lease, completion) = super::response_lifecycle(route_class, permit);
            lease.observe_response_permit_releases(Arc::clone(&policy_releases));
            AdmissionDecision::Admit {
                completion,
                grant: IngressGrant::new(lease),
                read_deadline: head.read_deadline_after(Duration::from_secs(30)),
            }
        });

        let prepared = begin_ingress(&app, "/admission", MonotonicInstant::now());
        let request = request_builder()
            .method(Method::GET)
            .uri("/admission")
            .body(Body::empty())
            .expect("request");
        let envelope = block_on(app.dispatch_admitted(prepared, request));

        assert_eq!(releases.load(Ordering::SeqCst), 0);
        let response = complete_envelope(envelope);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn composed_response_completion() {
        let static_effects = Arc::new(AtomicUsize::new(0));
        let late_bound_effects = Arc::new(AtomicUsize::new(0));
        let static_effects_for_policy = Arc::clone(&static_effects);
        let late_bound_effects_for_policy = Arc::clone(&late_bound_effects);
        let mut app = EdgeZeroApp::build::<super::App>(edgezero_core::PlatformMetadata::default())
            .expect("configured app");
        app.set_ingress_admission_policy(move |head| {
            let static_permit = super::ResponsePermit {
                release_counter: Some(Arc::clone(&static_effects_for_policy)),
                route_class: None,
            };
            let (mut lease, completion) = super::response_lifecycle(None, static_permit);
            lease.observe_response_permit_releases(Arc::clone(&late_bound_effects_for_policy));
            lease
                .install_response_permit()
                .expect("install late-bound response permit");
            AdmissionDecision::Admit {
                completion,
                grant: IngressGrant::empty(),
                read_deadline: head.read_deadline_after(Duration::from_secs(30)),
            }
        });

        let prepared_ingress = begin_ingress(&app, "/", MonotonicInstant::now());
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let envelope = block_on(app.dispatch_admitted(prepared_ingress, request));

        assert_eq!(static_effects.load(Ordering::SeqCst), 0);
        assert_eq!(late_bound_effects.load(Ordering::SeqCst), 0);
        let Ok((prepared_response, _, mut attempt, clock)) = envelope.begin() else {
            panic!("response egress begin");
        };
        assert_eq!(static_effects.load(Ordering::SeqCst), 0);
        assert_eq!(late_bound_effects.load(Ordering::SeqCst), 0);

        assert!(attempt.begin_writing());
        assert_eq!(static_effects.load(Ordering::SeqCst), 0);
        assert_eq!(late_bound_effects.load(Ordering::SeqCst), 0);

        let observed_at = clock.now();
        assert!(attempt.complete(observed_at));
        assert_eq!(static_effects.load(Ordering::SeqCst), 1);
        assert_eq!(late_bound_effects.load(Ordering::SeqCst), 1);

        assert!(!attempt.complete(observed_at));
        assert_eq!(static_effects.load(Ordering::SeqCst), 1);
        assert_eq!(late_bound_effects.load(Ordering::SeqCst), 1);
        drop(attempt);

        assert_eq!(static_effects.load(Ordering::SeqCst), 1);
        assert_eq!(late_bound_effects.load(Ordering::SeqCst), 1);
        assert_eq!(prepared_response.into_response().status(), StatusCode::OK);
    }

    #[test]
    fn configured_admission_preserves_fallback_status_at_exact_cap() {
        let app = EdgeZeroApp::build::<super::App>(edgezero_core::PlatformMetadata::default())
            .expect("configured app");
        for (method, path, expected) in [
            (Method::POST, "/missing", StatusCode::NOT_FOUND),
            (Method::POST, "/", StatusCode::METHOD_NOT_ALLOWED),
        ] {
            let request = request_builder()
                .method(method)
                .uri(path)
                .body(Body::stream(iter([Bytes::from(vec![b'a'; 4_096])])))
                .expect("request");
            let response = complete_dispatch(
                block_on(app.dispatch_ingress(
                    request,
                    MonotonicInstant::now(),
                    edgezero_core::IngressHeadAccounting::HostManaged,
                    edgezero_core::IngressFraming::HostManaged,
                ))
                .expect("dispatch"),
            );
            assert_eq!(response.status(), expected);
        }
    }

    #[test]
    fn configured_admission_returns_exact_overflow_response_for_both_fallbacks() {
        let app = EdgeZeroApp::build::<super::App>(edgezero_core::PlatformMetadata::default())
            .expect("configured app");
        for (method, path) in [(Method::POST, "/missing"), (Method::POST, "/")] {
            let request = request_builder()
                .method(method)
                .uri(path)
                .body(Body::stream(iter([
                    Bytes::from(vec![b'a'; 4_096]),
                    Bytes::from_static(b"b"),
                ])))
                .expect("request");
            let response = complete_dispatch(
                block_on(app.dispatch_ingress(
                    request,
                    MonotonicInstant::now(),
                    edgezero_core::IngressHeadAccounting::HostManaged,
                    edgezero_core::IngressFraming::HostManaged,
                ))
                .expect("dispatch"),
            );

            assert_plain_text_response(
                &response,
                StatusCode::BAD_REQUEST,
                b"request body too large\n",
            );
        }
    }

    #[test]
    fn configured_admission_returns_exact_timeout_response() {
        let app = EdgeZeroApp::build::<super::App>(edgezero_core::PlatformMetadata::default())
            .expect("configured app");
        let request = request_builder()
            .method(Method::POST)
            .uri("/missing")
            .body(Body::from_stream(iter([Err::<Bytes, _>(
                EdgeError::request_timeout("adapter read deadline exceeded"),
            )])))
            .expect("request");
        let response = complete_dispatch(
            block_on(app.dispatch_ingress(
                request,
                MonotonicInstant::now(),
                edgezero_core::IngressHeadAccounting::HostManaged,
                edgezero_core::IngressFraming::HostManaged,
            ))
            .expect("dispatch"),
        );

        assert_plain_text_response(&response, StatusCode::REQUEST_TIMEOUT, b"request timeout\n");
    }

    fn assert_plain_text_response(response: &Response, status: StatusCode, body: &[u8]) {
        let expected_length = body.len().to_string();
        assert_eq!(response.status(), status);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .expect("content type"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            response
                .headers()
                .get("content-length")
                .expect("content length")
                .to_str()
                .expect("ASCII content length"),
            expected_length
        );
        assert_eq!(response.body().as_bytes().expect("buffered body"), body);
    }

    fn complete_dispatch(outcome: IngressDispatchOutcome) -> Response {
        let IngressDispatchOutcome::Response(envelope) = outcome else {
            panic!("expected response");
        };
        complete_envelope(*envelope)
    }

    fn complete_envelope(envelope: ResponseEgressEnvelope) -> Response {
        let Ok((prepared, _, mut attempt, clock)) = envelope.begin() else {
            panic!("response egress begin");
        };
        assert!(attempt.begin_writing());
        assert!(attempt.complete(clock.now()));
        prepared.into_response()
    }

    fn begin_ingress(
        app: &EdgeZeroApp,
        path: &str,
        start: MonotonicInstant,
    ) -> edgezero_core::PreparedIngress {
        let head = IngressHeadParts::new(
            Method::GET,
            path.parse().expect("URI"),
            Version::HTTP_11,
            HeaderMap::new(),
        );
        match app.begin_ingress(head, start).expect("begin ingress") {
            IngressBeginOutcome::Admitted(prepared) => prepared,
            IngressBeginOutcome::Refused(_) => panic!("demo policy must admit request"),
            IngressBeginOutcome::Aborted | _ => panic!("unknown admission outcome"),
        }
    }
}
