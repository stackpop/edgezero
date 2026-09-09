//! Core primitives for building portable edge workloads across edge adapters.

// Targets a single line — the proc-macro re-export at the bottom of this
// file. The `pub_use` lint is module-scoped (cannot be `#[expect]`-ed
// per-item), and proc-macros must be re-exported here so downstream users
// depend only on `edgezero-core` (not `edgezero-macros`).
#![expect(
    clippy::pub_use,
    reason = "proc-macros must be re-exported through the parent crate"
)]

// Required so `#[action]` handlers defined inside this crate resolve the
// absolute `::edgezero_core::…` paths the proc-macro emits.
extern crate self as edgezero_core;

pub mod addr;
pub mod app;
pub mod app_config;
pub mod blob_envelope;
pub mod body;
pub mod canonical_form;
pub mod compression;
pub mod config_store;
pub mod context;
pub mod env_config;
pub mod error;
pub mod extractor;
pub mod handler;
pub mod http;
pub mod ingress;
pub mod introspection;
pub mod key_value_store;
pub mod manifest;
pub mod middleware;
pub mod outbound;
pub mod params;
pub mod responder;
pub mod response;
pub mod router;
pub mod secret_store;
pub mod store_registry;
/// Test-only env-var guards. The workspace's only `unsafe` lives here; see the
/// module docs. Enable via the `test-utils` feature in `[dev-dependencies]`.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_env;
pub mod time;

pub use body::{Body, BodyStream};
pub use compression::{
    BROTLI_DECODER_FIXED_CHARGE_BYTES, ContentEncoding, brotli_decoder_memory_charge,
    classify_content_encoding, decode_brotli_stream, decode_gzip_stream,
};
pub use edgezero_macros::{AppConfig, action, app};
pub use error::{
    BadGatewayDecodeReason, BadGatewayReason, BudgetSource, EdgeError, ResponseLimitReason,
};
pub use ingress::{
    AdmissionDecision, AdmittedIngress, DEFAULT_INBOUND_READ_BUDGET,
    DEFAULT_MAX_REQUEST_HEADER_BYTES, DEFAULT_MAX_REQUEST_HEADER_COUNT,
    DEFAULT_MAX_REQUEST_TARGET_BYTES, IngressAdmissionOutcome, IngressFraming, IngressGrant,
    IngressHead, IngressHeadAccounting, IngressHeadLimits,
};
pub use manifest::{
    AtomicHost, BakedManifest, Capability, CapabilitySupport, HostParseError, HostPat,
    ManifestCapabilities, ManifestContract, ManifestOutboundCapability, Port, Scheme,
    canonicalize_outbound_host,
};
pub use outbound::{
    DEFAULT_MAX_BROTLI_DECODER_BYTES, DEFAULT_MAX_RESPONSE_BYTES,
    DEFAULT_OUTBOUND_REQUEST_BODY_BYTES, HttpClient, OutboundHttpClient, OutboundRequest,
    OutboundRequestParts, OutboundResponse, OutboundSlotResult, PROXY_HEADER,
    ResponseBodyDisposition, ResponseHeaderLimiter, ResponseMode, collect_response_stream,
    enforce_payload_content_length, limit_decoded_stream, limit_encoded_stream,
    normalize_for_dispatch, normalize_response_headers, rechunk_stream, validate_for_dispatch,
};
pub use router::{ResolvedDispatch, RouteId, RouteInfo, RouteMetadata, RouteResolution};
pub use time::{Deadline, DispatchBudget, MonotonicInstant, dispatch_budget};
