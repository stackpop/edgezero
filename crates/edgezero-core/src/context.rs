use std::{cell::RefCell, mem};

use crate::body::Body;
use crate::config_store::ConfigExtractionLimits;
use crate::error::{
    BadGatewayReason, BudgetSource, EdgeError, ResponseLimitReason, StoreExtractionReason,
};
use crate::http::{Extensions, HeaderMap, Method, Request, RequestParts, Uri, Version};
use crate::ingress::{AdmittedIngress, IngressGrant};
use crate::outbound::HttpClient;
use crate::params::PathParams;
use crate::router::RouteMetadata;
use crate::store_registry::{
    BoundConfigStore, BoundKvStore, BoundSecretStore, ConfigRegistry, ConfigStoreBinding,
    KvRegistry, SecretRegistry, StoreRegistry,
};
use crate::time::{Deadline, MonotonicClock, MonotonicInstant};
use futures_util::StreamExt as _;
use serde::de::DeserializeOwned;

/// Default maximum body size accepted by JSON extractors (8 MiB).
pub const DEFAULT_INBOUND_JSON_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum body size accepted by form extractors (1 MiB).
pub const DEFAULT_INBOUND_FORM_BYTES: usize = 1024 * 1024;

/// Non-consuming snapshot of inbound body ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BodyKind {
    Cached { len: usize },
    Draining,
    Initial,
    Poisoned,
    Taken,
}

enum BodyState {
    Cached(bytes::Bytes),
    Draining,
    Initial(Body),
    Poisoned(StoredError),
    Taken,
}

pub(crate) enum FallbackDrainOutcome {
    Complete,
    Exceeded,
    TimedOut,
}

enum StoredError {
    BadGateway(String, BadGatewayReason),
    BadRequest(String),
    ConfigOutOfDate(String, String),
    GatewayTimeout(String, BudgetSource),
    Internal(String),
    MethodNotAllowed(Method, String),
    NotFound(String),
    NotImplemented(String),
    RequestHeaderFieldsTooLarge(String),
    RequestTimeout(String),
    ResponseTooLarge(String, ResponseLimitReason),
    ServiceUnavailable(String),
    StoreExtraction(StoreExtractionReason, String, Option<String>),
    UriTooLong(String),
    Validation(String),
}

impl StoredError {
    fn cancelled() -> Self {
        Self::Internal("inbound body drain cancelled".to_owned())
    }

    fn capture(error: EdgeError) -> Self {
        match error {
            EdgeError::BadGateway { message, reason } => Self::BadGateway(message, reason),
            EdgeError::BadRequest { message } => Self::BadRequest(message),
            EdgeError::ConfigOutOfDate {
                message,
                field_path,
            } => Self::ConfigOutOfDate(message, field_path),
            EdgeError::GatewayTimeout { message, cause } => Self::GatewayTimeout(message, cause),
            EdgeError::Internal { source } => Self::Internal(source.to_string()),
            EdgeError::MethodNotAllowed { method, allowed } => {
                Self::MethodNotAllowed(method, allowed)
            }
            EdgeError::NotFound { path } => Self::NotFound(path),
            EdgeError::NotImplemented { message } => Self::NotImplemented(message),
            EdgeError::RequestHeaderFieldsTooLarge { message } => {
                Self::RequestHeaderFieldsTooLarge(message)
            }
            EdgeError::RequestTimeout { message } => Self::RequestTimeout(message),
            EdgeError::ResponseTooLarge { message, reason } => {
                Self::ResponseTooLarge(message, reason)
            }
            EdgeError::ServiceUnavailable { message } => Self::ServiceUnavailable(message),
            EdgeError::StoreExtraction {
                reason,
                message,
                field_path,
            } => Self::StoreExtraction(reason, message, field_path),
            EdgeError::UriTooLong { message } => Self::UriTooLong(message),
            EdgeError::Validation { message } => Self::Validation(message),
        }
    }

    fn to_edge_error(&self) -> EdgeError {
        match self {
            Self::BadGateway(message, reason) => {
                EdgeError::bad_gateway_with_reason(message.clone(), *reason)
            }
            Self::BadRequest(message) => EdgeError::bad_request(message.clone()),
            Self::ConfigOutOfDate(message, field_path) => {
                EdgeError::config_out_of_date(message.clone(), field_path.clone())
            }
            Self::GatewayTimeout(message, cause) => {
                EdgeError::gateway_timeout_caused(message.clone(), *cause)
            }
            Self::Internal(rendered) => EdgeError::internal(anyhow::anyhow!(rendered.clone())),
            Self::MethodNotAllowed(method, allowed) => EdgeError::MethodNotAllowed {
                method: method.clone(),
                allowed: allowed.clone(),
            },
            Self::NotFound(path) => EdgeError::not_found(path.clone()),
            Self::NotImplemented(message) => EdgeError::not_implemented(message.clone()),
            Self::RequestHeaderFieldsTooLarge(message) => {
                EdgeError::request_header_fields_too_large(message.clone())
            }
            Self::RequestTimeout(message) => EdgeError::request_timeout(message.clone()),
            Self::ResponseTooLarge(message, reason) => {
                EdgeError::response_too_large_with_reason(message.clone(), *reason)
            }
            Self::ServiceUnavailable(message) => EdgeError::service_unavailable(message.clone()),
            Self::StoreExtraction(reason, message, field_path) => {
                EdgeError::store_extraction(*reason, message.clone(), field_path.clone())
            }
            Self::UriTooLong(message) => EdgeError::uri_too_long(message.clone()),
            Self::Validation(message) => EdgeError::validation(message.clone()),
        }
    }
}

/// Request context exposed to handlers and middleware.
pub struct RequestContext {
    body: RefCell<BodyState>,
    config_extraction_limits: ConfigExtractionLimits,
    ingress_grant: RefCell<Option<IngressGrant>>,
    monotonic_clock: MonotonicClock,
    parts: RequestParts,
    path_params: PathParams,
    read_deadline: Option<Deadline>,
    request_start: MonotonicInstant,
    route_metadata: Option<RouteMetadata>,
}

struct DrainGuard<'cell> {
    armed: bool,
    body: &'cell RefCell<BodyState>,
}

impl DrainGuard<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DrainGuard<'_> {
    fn drop(&mut self) {
        if self.armed
            && let Ok(mut state) = self.body.try_borrow_mut()
        {
            *state = BodyState::Poisoned(StoredError::cancelled());
        }
    }
}

impl RequestContext {
    /// Drains and caches the inbound body under the caller's byte cap.
    ///
    /// # Errors
    /// Returns 400 on overflow, 408 on an admitted read deadline, and preserves a sticky
    /// source error for every later accessor.
    #[inline]
    pub async fn body_bytes(&self, max: usize) -> Result<bytes::Bytes, EdgeError> {
        let body = {
            let mut state = self.body.borrow_mut();
            match mem::replace(&mut *state, BodyState::Draining) {
                BodyState::Cached(bytes) => {
                    let result = check_cached_body(&bytes, max);
                    *state = BodyState::Cached(bytes);
                    return result;
                }
                BodyState::Draining => {
                    *state = BodyState::Draining;
                    return Err(EdgeError::internal(anyhow::anyhow!(
                        "body read already in progress"
                    )));
                }
                BodyState::Initial(body) => body,
                BodyState::Poisoned(error) => {
                    let returned = error.to_edge_error();
                    *state = BodyState::Poisoned(error);
                    return Err(returned);
                }
                BodyState::Taken => {
                    *state = BodyState::Taken;
                    return Err(EdgeError::internal(anyhow::anyhow!(
                        "body already consumed via take_body"
                    )));
                }
            }
        };

        let mut guard = DrainGuard {
            armed: true,
            body: &self.body,
        };
        let result = drain_body(body, max, self.read_deadline, &self.monotonic_clock).await;
        match result {
            Ok(bytes) => {
                *self.body.borrow_mut() = BodyState::Cached(bytes.clone());
                guard.disarm();
                Ok(bytes)
            }
            Err(error) => {
                let stored = StoredError::capture(error);
                let returned = stored.to_edge_error();
                *self.body.borrow_mut() = BodyState::Poisoned(stored);
                guard.disarm();
                Err(returned)
            }
        }
    }

    #[must_use]
    #[inline]
    pub fn body_kind(&self) -> BodyKind {
        match &*self.body.borrow() {
            BodyState::Cached(bytes) => BodyKind::Cached { len: bytes.len() },
            BodyState::Draining => BodyKind::Draining,
            BodyState::Initial(_) => BodyKind::Initial,
            BodyState::Poisoned(_) => BodyKind::Poisoned,
            BodyState::Taken => BodyKind::Taken,
        }
    }

    #[must_use]
    #[inline]
    pub fn config_extraction_limits(&self) -> ConfigExtractionLimits {
        self.config_extraction_limits
    }

    /// Resolve the [`BoundConfigStore`] for `id`. Strict lookup: when a
    /// [`ConfigRegistry`] is wired, an unregistered id yields `None`. When
    /// no registry is wired this returns `None` — adapter dispatchers
    /// normalise legacy bare-handle inputs to a single-id registry under
    /// the conventional `"default"` id, so a missing registry is a real
    /// bug rather than a hand-wired single-handle adapter (spec hard-cutoff).
    #[inline]
    pub fn config_store(&self, id: &str) -> Option<BoundConfigStore> {
        self.parts
            .extensions
            .get::<ConfigRegistry>()
            .and_then(|registry| registry.named(id))
            .map(|binding| binding.handle)
    }

    /// Borrow a named binding.
    #[must_use]
    #[inline]
    pub fn config_store_binding(&self, id: &str) -> Option<&ConfigStoreBinding> {
        let registry = self.parts.extensions.get::<ConfigRegistry>()?;
        registry.named_ref(id)
    }

    /// Resolve the default [`BoundConfigStore`] — the wired registry's
    /// declared default id, or `None` when no registry is in extensions.
    /// See [`Self::config_store`] for the hard-cutoff rationale.
    #[inline]
    pub fn config_store_default(&self) -> Option<BoundConfigStore> {
        self.parts
            .extensions
            .get::<ConfigRegistry>()
            .and_then(StoreRegistry::default)
            .map(|binding| binding.handle)
    }

    /// Borrow the default config-store binding (handle + key). See
    /// spec 5.2.1.
    #[must_use]
    #[inline]
    pub fn config_store_default_binding(&self) -> Option<&ConfigStoreBinding> {
        let registry = self.parts.extensions.get::<ConfigRegistry>()?;
        registry.default_ref()
    }

    /// Clone a request extension of type `T`, if present. Used by the
    /// introspection extractors (`ManifestJson` / `RouteTable`) to read the
    /// payload the router injected for their route.
    #[must_use]
    #[inline]
    pub(crate) fn extension<T>(&self) -> Option<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        self.parts.extensions.get::<T>().cloned()
    }

    #[must_use]
    #[inline]
    pub fn extensions(&self) -> &Extensions {
        &self.parts.extensions
    }

    #[inline]
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.parts.extensions
    }

    /// Buffers at most `max` bytes and deserializes form-urlencoded data.
    ///
    /// # Errors
    /// Returns 400 when the body is oversized or malformed, and preserves body drain errors.
    #[inline]
    pub async fn form_within<T>(&self, max: usize) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        let bytes = self.body_bytes(max).await?;
        serde_urlencoded::from_bytes(bytes.as_ref())
            .map_err(|err| EdgeError::bad_request(format!("invalid form payload: {err}")))
    }

    #[must_use]
    #[inline]
    pub fn headers(&self) -> &HeaderMap {
        &self.parts.headers
    }

    #[inline]
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.parts.headers
    }

    #[must_use]
    #[inline]
    pub fn http_client(&self) -> Option<HttpClient> {
        self.parts.extensions.get::<HttpClient>().cloned()
    }

    /// Reassembles the request while preserving an initial or cached body.
    ///
    /// # Errors
    /// Returns the sticky body error or an internal error if a drain is in progress.
    #[inline]
    pub fn into_request(self) -> Result<Request, EdgeError> {
        let body = match self.body.into_inner() {
            BodyState::Cached(bytes) => Body::from(bytes),
            BodyState::Draining => {
                return Err(EdgeError::internal(anyhow::anyhow!(
                    "body read in progress"
                )));
            }
            BodyState::Initial(body) => body,
            BodyState::Poisoned(error) => return Err(error.to_edge_error()),
            BodyState::Taken => Body::empty(),
        };
        Ok(Request::from_parts(self.parts, body))
    }

    /// Buffers at most `max` bytes and deserializes JSON.
    ///
    /// # Errors
    /// Returns 400 when the body is oversized or malformed, and preserves body drain errors.
    #[inline]
    pub async fn json_within<T>(&self, max: usize) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        let bytes = self.body_bytes(max).await?;
        serde_json::from_slice(bytes.as_ref())
            .map_err(|err| EdgeError::bad_request(format!("invalid JSON payload: {err}")))
    }

    /// Resolve the [`BoundKvStore`] for `id`. Strict lookup: when a
    /// [`KvRegistry`] is wired, an unregistered id yields `None`. When no
    /// registry is wired this returns `None` — adapter dispatchers
    /// normalise legacy bare-handle inputs to a single-id registry under
    /// the conventional `"default"` id (spec hard-cutoff).
    #[inline]
    pub fn kv_store(&self, id: &str) -> Option<BoundKvStore> {
        let registry = self.parts.extensions.get::<KvRegistry>()?;
        registry.named(id)
    }

    /// Resolve the default [`BoundKvStore`] — the wired registry's
    /// declared default id, or `None` when no registry is in extensions.
    /// See [`Self::kv_store`] for the hard-cutoff rationale.
    #[inline]
    pub fn kv_store_default(&self) -> Option<BoundKvStore> {
        let registry = self.parts.extensions.get::<KvRegistry>()?;
        registry.default()
    }

    #[must_use]
    #[inline]
    pub fn method(&self) -> &Method {
        &self.parts.method
    }

    /// Returns the clock paired with this request's admitted ingress lifetime.
    #[must_use]
    #[inline]
    pub fn monotonic_clock(&self) -> MonotonicClock {
        self.monotonic_clock.clone()
    }

    #[inline]
    pub fn new(request: Request, params: PathParams) -> Self {
        let (parts, body) = request.into_parts();
        Self {
            body: RefCell::new(BodyState::Initial(body)),
            config_extraction_limits: ConfigExtractionLimits::default(),
            ingress_grant: RefCell::new(None),
            monotonic_clock: MonotonicClock::default(),
            parts,
            path_params: params,
            read_deadline: None,
            request_start: MonotonicInstant::now(),
            route_metadata: None,
        }
    }

    pub(crate) fn new_routed(
        request: Request,
        params: PathParams,
        route_metadata: RouteMetadata,
        ingress: AdmittedIngress,
    ) -> Self {
        let (request_start, read_deadline, grant, config_extraction_limits, monotonic_clock) =
            ingress.into_parts();
        let (parts, body) = request.into_parts();
        Self {
            body: RefCell::new(BodyState::Initial(body)),
            config_extraction_limits,
            ingress_grant: RefCell::new(Some(grant)),
            monotonic_clock,
            parts,
            path_params: params,
            read_deadline: Some(read_deadline),
            request_start,
            route_metadata: Some(route_metadata),
        }
    }

    #[must_use]
    #[inline]
    pub fn parts(&self) -> &RequestParts {
        &self.parts
    }

    #[inline]
    pub fn parts_mut(&mut self) -> &mut RequestParts {
        &mut self.parts
    }

    /// # Errors
    /// Returns [`EdgeError::bad_request`] if the path parameters cannot be deserialized into `T`.
    #[inline]
    pub fn path<T>(&self) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        self.path_params
            .deserialize()
            .map_err(|err| EdgeError::bad_request(format!("invalid path parameters: {err}")))
    }

    #[inline]
    pub fn path_params(&self) -> &PathParams {
        &self.path_params
    }

    /// # Errors
    /// Returns [`EdgeError::bad_request`] if the query string cannot be deserialized into `T`.
    #[inline]
    pub fn query<T>(&self) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        let query = self.parts.uri.query().unwrap_or("");
        serde_urlencoded::from_str(query)
            .map_err(|err| EdgeError::bad_request(format!("invalid query string: {err}")))
    }

    /// Absolute deadline governing the admitted inbound body, if this context entered
    /// through the adapter admission seam.
    #[must_use]
    #[inline]
    pub fn read_deadline(&self) -> Option<Deadline> {
        self.read_deadline
    }

    /// Monotonic instant captured at ingress entry or low-level context construction.
    #[must_use]
    #[inline]
    pub fn request_start(&self) -> MonotonicInstant {
        self.request_start
    }

    /// Canonical matched route metadata, absent for low-level contexts.
    #[must_use]
    #[inline]
    pub fn route_metadata(&self) -> Option<&RouteMetadata> {
        self.route_metadata.as_ref()
    }

    /// Resolve the [`BoundSecretStore`] for `id`. Strict lookup: when a
    /// [`SecretRegistry`] is wired, an unregistered id yields `None`.
    /// When no registry is wired this returns `None` — adapter
    /// dispatchers normalise legacy bare-handle inputs to a single-id
    /// registry under the conventional `"default"` id (spec hard-cutoff).
    #[inline]
    pub fn secret_store(&self, id: &str) -> Option<BoundSecretStore> {
        let registry = self.parts.extensions.get::<SecretRegistry>()?;
        registry.named(id)
    }

    /// Resolve the default [`BoundSecretStore`] — the wired registry's
    /// declared default id, or `None` when no registry is in extensions.
    /// See [`Self::secret_store`] for the hard-cutoff rationale.
    #[inline]
    pub fn secret_store_default(&self) -> Option<BoundSecretStore> {
        let registry = self.parts.extensions.get::<SecretRegistry>()?;
        registry.default()
    }

    /// Consumes body ownership without buffering it.
    ///
    /// # Errors
    /// Returns the sticky body error or an internal error if a drain is in progress.
    #[inline]
    pub fn take_body(&self) -> Result<Body, EdgeError> {
        let mut state = self.body.borrow_mut();
        match mem::replace(&mut *state, BodyState::Taken) {
            BodyState::Cached(bytes) => Ok(Body::from(bytes)),
            BodyState::Draining => {
                *state = BodyState::Draining;
                Err(EdgeError::internal(anyhow::anyhow!(
                    "body read in progress"
                )))
            }
            BodyState::Initial(body) => Ok(body),
            BodyState::Poisoned(error) => {
                let returned = error.to_edge_error();
                *state = BodyState::Poisoned(error);
                Err(returned)
            }
            BodyState::Taken => {
                *state = BodyState::Taken;
                Ok(Body::empty())
            }
        }
    }

    /// Takes the application-owned ingress grant at most once.
    #[must_use]
    #[inline]
    pub fn take_ingress_grant(&self) -> Option<IngressGrant> {
        self.ingress_grant.borrow_mut().take()
    }

    #[must_use]
    #[inline]
    pub fn uri(&self) -> &Uri {
        &self.parts.uri
    }

    #[must_use]
    #[inline]
    pub fn version(&self) -> Version {
        self.parts.version
    }
}

fn check_cached_body(bytes: &bytes::Bytes, max: usize) -> Result<bytes::Bytes, EdgeError> {
    if bytes.len() > max {
        return Err(EdgeError::bad_request("request body too large"));
    }
    Ok(bytes.clone())
}

fn check_read_deadline(
    deadline: Option<Deadline>,
    monotonic_clock: &MonotonicClock,
) -> Result<(), EdgeError> {
    if deadline.is_some_and(|candidate| candidate.is_expired_at(monotonic_clock.now())) {
        return Err(EdgeError::request_timeout(
            "inbound body read deadline exceeded",
        ));
    }
    Ok(())
}

pub(crate) async fn drain_body(
    body: Body,
    max: usize,
    deadline: Option<Deadline>,
    monotonic_clock: &MonotonicClock,
) -> Result<bytes::Bytes, EdgeError> {
    check_read_deadline(deadline, monotonic_clock)?;
    match body {
        Body::Once(bytes) => {
            check_read_deadline(deadline, monotonic_clock)?;
            check_cached_body(&bytes, max)
        }
        Body::Stream(mut stream) => {
            let mut buffered = Vec::new();
            loop {
                check_read_deadline(deadline, monotonic_clock)?;
                let next = stream.next().await;
                // Deadline wins a simultaneous body/error/EOF observation.
                check_read_deadline(deadline, monotonic_clock)?;
                let Some(result) = next else {
                    return Ok(bytes::Bytes::from(buffered));
                };
                let chunk = result?;
                let next_len = buffered.len().checked_add(chunk.len()).ok_or_else(|| {
                    EdgeError::bad_request("request body size accounting overflow")
                })?;
                if next_len > max {
                    return Err(EdgeError::bad_request("request body too large"));
                }
                buffered.extend_from_slice(&chunk);
            }
        }
    }
}

fn fallback_deadline_outcome(
    deadline: Deadline,
    monotonic_clock: &MonotonicClock,
) -> Option<FallbackDrainOutcome> {
    deadline
        .is_expired_at(monotonic_clock.now())
        .then_some(FallbackDrainOutcome::TimedOut)
}

pub(crate) async fn drain_body_discard(
    body: Body,
    max: usize,
    deadline: Deadline,
    monotonic_clock: &MonotonicClock,
) -> Result<FallbackDrainOutcome, EdgeError> {
    if let Some(outcome) = fallback_deadline_outcome(deadline, monotonic_clock) {
        return Ok(outcome);
    }
    match body {
        Body::Once(bytes) => {
            if let Some(outcome) = fallback_deadline_outcome(deadline, monotonic_clock) {
                return Ok(outcome);
            }
            if bytes.len() > max {
                return Ok(FallbackDrainOutcome::Exceeded);
            }
            Ok(FallbackDrainOutcome::Complete)
        }
        Body::Stream(mut stream) => {
            let mut consumed = 0_usize;
            loop {
                if let Some(outcome) = fallback_deadline_outcome(deadline, monotonic_clock) {
                    return Ok(outcome);
                }
                let next = stream.next().await;
                // Deadline wins a simultaneous body/error/EOF observation.
                if let Some(outcome) = fallback_deadline_outcome(deadline, monotonic_clock) {
                    return Ok(outcome);
                }
                let Some(result) = next else {
                    return Ok(FallbackDrainOutcome::Complete);
                };
                let chunk = match result {
                    Ok(chunk) => chunk,
                    Err(EdgeError::RequestTimeout { .. }) => {
                        return Ok(FallbackDrainOutcome::TimedOut);
                    }
                    Err(error) => return Err(error),
                };
                let Some(next_consumed) = consumed.checked_add(chunk.len()) else {
                    return Ok(FallbackDrainOutcome::Exceeded);
                };
                consumed = next_consumed;
                if consumed > max {
                    return Ok(FallbackDrainOutcome::Exceeded);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::missing_trait_methods,
        reason = "legacy provider stubs intentionally exercise the bounded-read compatibility default"
    )]

    use super::*;
    use crate::http::{HeaderMap, HeaderValue, Method, StatusCode, request_builder};
    use crate::outbound::{
        HttpClient, OutboundHttpClient, OutboundRequest, OutboundResponse, OutboundSlotResult,
    };
    use crate::params::PathParams;
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::executor::block_on;
    use futures::stream;
    use futures::task::noop_waker_ref;
    use serde::{Deserialize, Serialize};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::future::Future as _;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;

    struct DummyOutboundClient;

    #[derive(Debug, PartialEq, Deserialize, Serialize)]
    struct PathData {
        id: String,
    }

    #[async_trait(?Send)]
    impl OutboundHttpClient for DummyOutboundClient {
        async fn send(&self, request: OutboundRequest) -> Result<OutboundResponse, EdgeError> {
            Ok(OutboundResponse::new(
                request.method().clone(),
                StatusCode::OK,
                HeaderMap::new(),
                Body::empty(),
            ))
        }

        async fn send_all(&self, _requests: Vec<OutboundRequest>) -> Vec<OutboundSlotResult> {
            Vec::new()
        }
    }

    fn ctx(path: &str, body: Body, params: PathParams) -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri(path)
            .body(body)
            .expect("request");
        RequestContext::new(request, params)
    }

    fn params(map: &[(&str, &str)]) -> PathParams {
        let inner = map
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>();
        PathParams::new(inner)
    }

    // `RequestContext::config_handle()` was removed. The
    // present/absent behaviour is now covered by
    // `config_store_*` tests against a wired `ConfigRegistry`.

    #[test]
    fn form_deserialises_successfully() {
        #[derive(Deserialize, PartialEq, Debug)]
        struct FormData {
            name: String,
        }
        let body = Body::from("name=demo");
        let ctx = ctx("/submit", body, PathParams::default());
        let parsed: FormData =
            block_on(ctx.form_within(DEFAULT_INBOUND_FORM_BYTES)).expect("form data");
        assert_eq!(
            parsed,
            FormData {
                name: "demo".into()
            }
        );
        let debug = format!("{parsed:?}");
        assert!(debug.contains("demo"));
    }

    #[test]
    fn body_bytes_drains_once_and_rechecks_each_callers_cap() {
        let polls = Rc::new(Cell::new(0_u8));
        let observed_polls = Rc::clone(&polls);
        let body = Body::from_stream(stream::poll_fn(move |_cx| {
            let current = observed_polls.get();
            observed_polls.set(current.saturating_add(1));
            match current {
                0 => Poll::Ready(Some(Ok(Bytes::from_static(b"ok")))),
                _ => Poll::Ready(None),
            }
        }));
        let ctx = ctx("/body", body, PathParams::default());

        assert_eq!(
            block_on(ctx.body_bytes(2)).expect("exact cap"),
            Bytes::from_static(b"ok")
        );
        let stricter = block_on(ctx.body_bytes(1)).expect_err("stricter cached cap");
        assert_eq!(stricter.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            block_on(ctx.body_bytes(2)).expect("cached body remains usable"),
            Bytes::from_static(b"ok")
        );
        assert_eq!(polls.get(), 2, "the source is drained exactly once");
    }

    #[test]
    fn initial_drain_overflow_is_sticky_and_never_retries_source() {
        let polls = Rc::new(Cell::new(0_u8));
        let observed_polls = Rc::clone(&polls);
        let body = Body::from_stream(stream::poll_fn(move |_cx| {
            observed_polls.set(observed_polls.get().saturating_add(1));
            Poll::Ready(Some(Ok(Bytes::from_static(b"too large"))))
        }));
        let ctx = ctx("/body", body, PathParams::default());

        let first = block_on(ctx.body_bytes(3)).expect_err("overflow");
        let second = block_on(ctx.body_bytes(usize::MAX)).expect_err("sticky overflow");
        assert_eq!(first.status(), StatusCode::BAD_REQUEST);
        assert_eq!(second.status(), first.status());
        assert_eq!(second.message(), first.message());
        assert_eq!(ctx.body_kind(), BodyKind::Poisoned);
        assert_eq!(polls.get(), 1, "a poisoned source must not be polled again");
        assert_eq!(
            ctx.take_body().expect_err("poison survives take").status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn source_error_replays_typed_fields_without_repolling() {
        let body = Body::from_stream(stream::iter([Err(EdgeError::bad_gateway_with_reason(
            "upstream body failed",
            BadGatewayReason::Transport,
        ))]));
        let ctx = ctx("/body", body, PathParams::default());

        for _ in 0_u8..2_u8 {
            let error = block_on(ctx.body_bytes(32)).expect_err("sticky source error");
            assert!(matches!(
                error,
                EdgeError::BadGateway {
                    reason: BadGatewayReason::Transport,
                    ..
                }
            ));
            assert_eq!(error.message(), "upstream body failed");
        }
    }

    #[test]
    fn cancelled_drain_poison_is_sticky() {
        let body = Body::from_stream(stream::poll_fn(|_cx| Poll::Pending));
        let ctx = ctx("/body", body, PathParams::default());
        let mut first = Box::pin(ctx.body_bytes(32));
        let mut task = Context::from_waker(noop_waker_ref());
        assert!(matches!(first.as_mut().poll(&mut task), Poll::Pending));
        assert_eq!(ctx.body_kind(), BodyKind::Draining);

        drop(first);
        assert_eq!(ctx.body_kind(), BodyKind::Poisoned);
        let error = block_on(ctx.body_bytes(32)).expect_err("cancel poison");
        assert_eq!(
            error.message(),
            "internal error: inbound body drain cancelled"
        );
    }

    #[test]
    fn reentrant_read_errors_without_disrupting_first_drain() {
        let step = Rc::new(Cell::new(0_u8));
        let observed_step = Rc::clone(&step);
        let body = Body::from_stream(stream::poll_fn(move |_cx| match observed_step.get() {
            0 => {
                observed_step.set(1);
                Poll::Pending
            }
            1 => {
                observed_step.set(2);
                Poll::Ready(Some(Ok(Bytes::from_static(b"ok"))))
            }
            _ => Poll::Ready(None),
        }));
        let ctx = ctx("/body", body, PathParams::default());
        let mut first = Box::pin(ctx.body_bytes(2));
        let mut task = Context::from_waker(noop_waker_ref());
        assert!(matches!(first.as_mut().poll(&mut task), Poll::Pending));

        let reentrant = block_on(ctx.body_bytes(2)).expect_err("reentrant read");
        assert_eq!(
            reentrant.message(),
            "internal error: body read already in progress"
        );
        let Poll::Ready(first_result) = first.as_mut().poll(&mut task) else {
            panic!("scripted drain should complete");
        };
        assert_eq!(first_result.expect("first read"), Bytes::from_static(b"ok"));
        drop(first);
        assert_eq!(ctx.body_kind(), BodyKind::Cached { len: 2 });
    }

    #[test]
    fn expired_read_deadline_wins_and_poison_is_request_timeout() {
        let mut ctx = ctx("/body", Body::from("available"), PathParams::default());
        ctx.read_deadline = Some(Deadline::after(Duration::ZERO));

        let first = block_on(ctx.body_bytes(32)).expect_err("expired");
        let second = block_on(ctx.body_bytes(32)).expect_err("sticky expired");
        assert_eq!(first.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(second.status(), StatusCode::REQUEST_TIMEOUT);
        assert!(matches!(second, EdgeError::RequestTimeout { .. }));
    }

    #[test]
    fn injected_clock_controls_read_deadline_and_sticky_poison() {
        let start = MonotonicInstant::now();
        let now = Arc::new(Mutex::new(start));
        let observed_now = Arc::clone(&now);
        let mut ctx = ctx("/body", Body::from("available"), PathParams::default());
        ctx.monotonic_clock =
            MonotonicClock::new(move || *observed_now.lock().expect("clock lock"));
        let deadline = start.checked_add(Duration::from_secs(1)).expect("deadline");
        ctx.read_deadline = Some(Deadline::at_instant(deadline));
        *now.lock().expect("clock lock") = deadline;

        let first = block_on(ctx.body_bytes(32)).expect_err("expired");
        let second = block_on(ctx.body_bytes(32)).expect_err("sticky expired");
        assert_eq!(first.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(second.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(ctx.body_kind(), BodyKind::Poisoned);
    }

    #[test]
    fn take_body_and_into_request_cover_initial_cached_and_taken() {
        let initial_stream = Body::from_stream(stream::iter([Ok(Bytes::from_static(b"stream"))]));
        let initial = ctx("/initial", initial_stream, PathParams::default());
        assert!(initial.take_body().expect("initial body").is_stream());
        assert!(
            initial
                .take_body()
                .expect("taken becomes empty")
                .into_bytes()
                .is_some_and(|body_bytes| body_bytes.is_empty())
        );
        assert_eq!(initial.body_kind(), BodyKind::Taken);

        let cached = ctx("/cached", Body::from("cached"), PathParams::default());
        assert_eq!(
            block_on(cached.body_bytes(6)).expect("cache"),
            Bytes::from_static(b"cached")
        );
        assert_eq!(
            cached.take_body().expect("cached body").into_bytes(),
            Some(Bytes::from_static(b"cached"))
        );

        let reassembled = ctx("/request", Body::from("body"), PathParams::default());
        assert_eq!(
            block_on(reassembled.body_bytes(4)).expect("cache"),
            Bytes::from_static(b"body")
        );
        let request = reassembled.into_request().expect("reassembled request");
        assert_eq!(request.uri().path(), "/request");
        assert_eq!(
            request.into_body().into_bytes(),
            Some(Bytes::from_static(b"body"))
        );
    }

    #[test]
    fn poisoned_into_request_returns_the_stored_error() {
        let ctx = ctx("/request", Body::from("oversized"), PathParams::default());
        let first = block_on(ctx.body_bytes(1)).expect_err("overflow");
        let later = ctx.into_request().expect_err("poisoned request");
        assert_eq!(later.status(), first.status());
        assert_eq!(later.message(), first.message());
    }

    #[test]
    fn form_streaming_body_is_bounded_and_supported() {
        let stream = stream::iter(vec![Ok::<Bytes, anyhow::Error>(Bytes::from("name=demo"))]);
        let body = Body::from_external_stream(stream);
        let ctx = ctx("/submit", body, PathParams::default());
        let parsed: serde_json::Value =
            block_on(ctx.form_within(DEFAULT_INBOUND_FORM_BYTES)).expect("form data");
        assert_eq!(
            parsed.get("name").and_then(|value| value.as_str()),
            Some("demo")
        );
    }

    #[test]
    fn form_value_deserialises_successfully() {
        let body = Body::from("name=demo");
        let ctx = ctx("/submit", body, PathParams::default());
        let parsed: serde_json::Value =
            block_on(ctx.form_within(DEFAULT_INBOUND_FORM_BYTES)).expect("form data");
        assert_eq!(
            parsed.get("name").and_then(|value| value.as_str()),
            Some("demo")
        );
    }

    #[test]
    fn invalid_form_returns_bad_request() {
        #[expect(dead_code, reason = "field exercised only via Deserialize")]
        #[derive(Debug, Deserialize)]
        struct FormData {
            age: u8,
        }
        let body = Body::from("age=not-a-number");
        let ctx = ctx("/submit", body, PathParams::default());
        let err = block_on(ctx.form_within::<FormData>(DEFAULT_INBOUND_FORM_BYTES))
            .expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid form payload"));
    }

    #[test]
    fn invalid_json_returns_bad_request() {
        let body = Body::from(&b"not json"[..]);
        let ctx = ctx("/echo", body, PathParams::default());
        let err = block_on(ctx.json_within::<serde_json::Value>(DEFAULT_INBOUND_JSON_BYTES))
            .expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid JSON payload"));
    }

    #[test]
    fn invalid_path_returns_bad_request() {
        #[expect(dead_code, reason = "field exercised only via Deserialize")]
        #[derive(Debug, Deserialize)]
        struct NumericPath {
            id: u32,
        }
        let debug = format!("{:?}", NumericPath { id: 0 });
        assert!(debug.contains('0'));
        let ctx = ctx("/items/foo", Body::empty(), params(&[("id", "foo")]));
        let err = ctx.path::<NumericPath>().expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid path parameters"));
    }

    #[test]
    fn invalid_query_returns_bad_request() {
        #[expect(dead_code, reason = "field exercised only via Deserialize")]
        #[derive(Debug, Deserialize)]
        struct Query {
            page: u8,
        }
        let debug = format!("{:?}", Query { page: 0 });
        assert!(debug.contains('0'));
        let ctx = ctx("/items?page=foo", Body::empty(), PathParams::default());
        let err = ctx.query::<Query>().expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid query string"));
    }

    #[test]
    fn json_deserialises_from_body() {
        #[derive(Debug, Deserialize, Serialize, PartialEq)]
        struct Payload {
            name: String,
        }
        let body = Body::json(&Payload {
            name: "demo".into(),
        })
        .expect("json body");
        let ctx = ctx("/echo", body, PathParams::default());
        let parsed: Payload =
            block_on(ctx.json_within(DEFAULT_INBOUND_JSON_BYTES)).expect("json payload");
        assert_eq!(
            parsed,
            Payload {
                name: "demo".into()
            }
        );
    }

    #[test]
    fn http_client_is_retrieved_when_present() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/outbound")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(HttpClient::with_client(DummyOutboundClient));

        let ctx = RequestContext::new(request, PathParams::default());
        assert!(ctx.http_client().is_some());
    }

    // `RequestContext::kv_handle()` was removed. The
    // present/absent behaviour is now covered by `kv_store_*`
    // tests against a wired `KvRegistry`.

    #[test]
    fn path_deserialises_successfully() {
        let ctx = ctx("/items/42", Body::empty(), params(&[("id", "42")]));
        let parsed: PathData = ctx.path().expect("path parameters");
        assert_eq!(parsed, PathData { id: "42".into() });
        let serialized = serde_json::to_string(&parsed).expect("serialize");
        assert!(serialized.contains("42"));
    }

    #[test]
    fn query_defaults_to_empty_when_missing() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Query {
            page: Option<u8>,
        }
        let ctx = ctx("/items", Body::empty(), PathParams::default());
        let parsed: Query = ctx.query().expect("query");
        assert_eq!(parsed.page, None);
    }

    #[test]
    fn query_deserialises_successfully() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Query {
            page: u8,
        }
        let ctx = ctx("/items?page=5", Body::empty(), PathParams::default());
        let parsed: Query = ctx.query().expect("query");
        assert_eq!(parsed, Query { page: 5 });
    }

    #[test]
    fn request_context_accessors_return_expected_values() {
        let mut ctx = ctx(
            "/items/123",
            Body::from("payload"),
            params(&[("id", "123")]),
        );
        assert_eq!(ctx.uri().path(), "/items/123");
        ctx.headers_mut()
            .insert("x-test", HeaderValue::from_static("value"));
        assert_eq!(
            ctx.headers()
                .get("x-test")
                .and_then(|value| value.to_str().ok()),
            Some("value")
        );
        assert_eq!(ctx.path_params().get("id"), Some("123"));
        assert_eq!(
            block_on(ctx.body_bytes(7)).expect("buffered"),
            Bytes::from_static(b"payload")
        );
        assert!(ctx.route_metadata().is_none());
        assert!(ctx.read_deadline().is_none());
        assert!(ctx.take_ingress_grant().is_none());

        let request = ctx.into_request().expect("request");
        assert_eq!(request.uri().path(), "/items/123");
    }

    // `RequestContext::secret_handle()` was removed. The
    // present/absent behaviour is now covered by `secret_store_*`
    // tests against a wired `SecretRegistry`.

    #[test]
    fn kv_store_resolves_named_handle_from_registry() {
        use crate::key_value_store::{KvHandle, NoopKvStore};
        use crate::store_registry::{KvRegistry, StoreRegistry};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let sessions = KvHandle::new(Arc::new(NoopKvStore));
        let cache = KvHandle::new(Arc::new(NoopKvStore));
        let by_id: BTreeMap<String, KvHandle> = [
            ("sessions".to_owned(), sessions),
            ("cache".to_owned(), cache),
        ]
        .into_iter()
        .collect();
        let registry: KvRegistry = StoreRegistry::new(by_id, "sessions".to_owned());

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/kv")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        assert!(ctx.kv_store("sessions").is_some());
        assert!(ctx.kv_store("cache").is_some());
        assert!(
            ctx.kv_store("unknown").is_none(),
            "registry lookups are strict: unknown ids must yield None"
        );
        assert!(ctx.kv_store_default().is_some());
    }

    #[test]
    fn kv_store_returns_none_when_only_legacy_handle_wired() {
        // Hard-cutoff: a bare `KvHandle` in extensions
        // is ignored by the registry-aware accessor. Adapter
        // dispatchers no longer insert bare handles — they
        // always synthesise a `KvRegistry` from any wired handle
        // first — so this code path only fires when a test or
        // callsite bypasses the dispatcher and inserts a bare
        // handle directly into extensions. The accessor must
        // surface that as a missing registry (None) rather than
        // silently upgrading.
        use crate::key_value_store::{KvHandle, NoopKvStore};
        use std::sync::Arc;

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/kv")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(KvHandle::new(Arc::new(NoopKvStore)));

        let ctx = RequestContext::new(request, PathParams::default());
        assert!(
            ctx.kv_store("anything").is_none(),
            "registry-aware accessor must not auto-upgrade a bare handle"
        );
        assert!(
            ctx.kv_store_default().is_none(),
            "registry-aware default accessor must not auto-upgrade a bare handle"
        );
    }

    #[test]
    fn config_store_resolves_named_handle_from_registry() {
        use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
        use crate::store_registry::{ConfigRegistry, ConfigStoreBinding, StoreRegistry};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        struct FixedStore(&'static str);
        #[async_trait(?Send)]
        impl ConfigStore for FixedStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some(self.0.to_owned()))
            }
        }

        let primary_handle = ConfigStoreHandle::new(Arc::new(FixedStore("primary")));
        let analytics_handle = ConfigStoreHandle::new(Arc::new(FixedStore("analytics")));
        let by_id: BTreeMap<String, ConfigStoreBinding> = [
            (
                "primary".to_owned(),
                ConfigStoreBinding {
                    handle: primary_handle,
                    default_key: "primary".to_owned(),
                },
            ),
            (
                "analytics".to_owned(),
                ConfigStoreBinding {
                    handle: analytics_handle,
                    default_key: "analytics".to_owned(),
                },
            ),
        ]
        .into_iter()
        .collect();
        let registry: ConfigRegistry = StoreRegistry::new(by_id, "primary".to_owned());

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        let resolved = ctx.config_store("analytics").expect("analytics handle");
        assert_eq!(
            block_on(resolved.get("key")).expect("config value"),
            Some("analytics".to_owned())
        );
        assert!(ctx.config_store("unknown").is_none());
        let default = ctx.config_store_default().expect("default handle");
        assert_eq!(
            block_on(default.get("key")).expect("default config value"),
            Some("primary".to_owned())
        );
    }

    #[test]
    fn secret_store_resolves_named_handle_from_registry() {
        use crate::secret_store::{NoopSecretStore, SecretHandle};
        use crate::store_registry::{BoundSecretStore, SecretRegistry, StoreRegistry};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let handle = SecretHandle::new(Arc::new(NoopSecretStore));
        let by_id: BTreeMap<String, BoundSecretStore> = [(
            "default".to_owned(),
            // The registry binds the logical id to the platform store name —
            // in production that's `EDGEZERO__STORES__SECRETS__DEFAULT__NAME`
            // resolved against the env (falling back to the logical id).
            BoundSecretStore::new(handle, "platform-secret-store".to_owned()),
        )]
        .into_iter()
        .collect();
        let registry: SecretRegistry = StoreRegistry::new(by_id, "default".to_owned());

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/secrets")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        let bound = ctx.secret_store("default").expect("default bound store");
        assert_eq!(bound.store_name(), "platform-secret-store");
        assert!(ctx.secret_store("unknown").is_none());
        assert!(ctx.secret_store_default().is_some());
    }

    #[test]
    fn secret_store_default_returns_none_when_only_legacy_handle_wired() {
        // Hard-cutoff: same semantics as
        // `kv_store_returns_none_when_only_legacy_handle_wired` —
        // a bare `SecretHandle` in extensions (a state that
        // only arises if a test bypasses the dispatcher) must
        // not auto-upgrade into a synthetic registry.
        use crate::secret_store::{NoopSecretStore, SecretHandle};
        use std::sync::Arc;

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/secrets")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(SecretHandle::new(Arc::new(NoopSecretStore)));

        let ctx = RequestContext::new(request, PathParams::default());
        assert!(
            ctx.secret_store_default().is_none(),
            "registry-aware default accessor must not auto-upgrade a bare handle"
        );
    }

    // -- RequestContext::config_store_default_binding / config_store_binding (B8) --

    #[test]
    fn config_store_default_binding_returns_binding_when_registry_present() {
        use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
        use crate::store_registry::{ConfigRegistry, ConfigStoreBinding, StoreRegistry};
        use std::sync::Arc;

        struct AnyStore;
        #[async_trait(?Send)]
        impl ConfigStore for AnyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(None)
            }
        }

        let binding = ConfigStoreBinding {
            handle: ConfigStoreHandle::new(Arc::new(AnyStore)),
            default_key: "resolved_key".to_owned(),
        };
        let registry: ConfigRegistry = StoreRegistry::single_id("app_config".to_owned(), binding);

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        let def = ctx.config_store_default_binding().expect("default binding");
        assert_eq!(def.default_key, "resolved_key");
    }

    #[test]
    fn config_store_default_binding_returns_none_when_no_registry() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        assert!(
            ctx.config_store_default_binding().is_none(),
            "no registry -- default binding must be None"
        );
    }

    #[test]
    fn config_store_binding_returns_named_binding_and_none_for_unknown() {
        use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
        use crate::store_registry::{ConfigRegistry, ConfigStoreBinding, StoreRegistry};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        struct AnyStore;
        #[async_trait(?Send)]
        impl ConfigStore for AnyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(None)
            }
        }

        let registry: ConfigRegistry = StoreRegistry::new(
            [
                (
                    "primary".to_owned(),
                    ConfigStoreBinding {
                        handle: ConfigStoreHandle::new(Arc::new(AnyStore)),
                        default_key: "pk".to_owned(),
                    },
                ),
                (
                    "secondary".to_owned(),
                    ConfigStoreBinding {
                        handle: ConfigStoreHandle::new(Arc::new(AnyStore)),
                        default_key: "sk".to_owned(),
                    },
                ),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            "primary".to_owned(),
        );

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());

        let sec = ctx
            .config_store_binding("secondary")
            .expect("secondary binding");
        assert_eq!(sec.default_key, "sk");

        assert!(
            ctx.config_store_binding("undeclared").is_none(),
            "unknown id must yield None"
        );
    }
}
