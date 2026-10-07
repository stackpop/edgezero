//! Lazy, successful-only state retention for custom Fastly dispatch.
//!
//! Only application-owned state belongs here. Native request handles, bodies,
//! metadata, extensions, and pending work must remain local to each callback.

#[cfg(feature = "fastly")]
use fastly::http::serve::{HandlerResult, Serve, ServeSummary};

/// State owned by one invocation of a custom serving entry point.
///
/// A new sandbox can start at any request. Initialization must therefore be
/// repeatable; retaining state never guarantees its lifetime or persistence.
pub struct Sandbox<T> {
    initialization_attempts: u64,
    requests: u64,
    setup_complete: bool,
    state: Option<T>,
}

#[cfg(any(feature = "fastly", test))]
struct DeliveryResult<E>(Result<(), E>);

#[cfg(feature = "fastly")]
impl<E> HandlerResult for DeliveryResult<E> {
    type Error = E;

    fn send(self) -> Result<(), E> {
        self.0
    }
}

impl<T> Default for Sandbox<T> {
    #[inline]
    fn default() -> Self {
        Self {
            state: None,
            requests: 0,
            initialization_attempts: 0,
            setup_complete: false,
        }
    }
}

#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "group accessors before lifecycle operations"
)]
impl<T> Sandbox<T> {
    /// Successfully initialized application state, if any.
    #[inline]
    pub fn state(&self) -> Option<&T> {
        self.state.as_ref()
    }

    /// Attempted callbacks, including the current callback and early returns.
    #[inline]
    pub fn requests(&self) -> u64 {
        self.requests
    }

    /// Builder invocations, including failed attempts.
    #[inline]
    pub fn initialization_attempts(&self) -> u64 {
        self.initialization_attempts
    }

    /// Build only when empty; retain only success and return errors unchanged.
    ///
    /// An error can carry a request-local fallback router. The callback decides
    /// how to respond and whether to continue serving. A later call retries an
    /// unsuccessful build. Panics propagate normally.
    ///
    /// # Errors
    /// Returns the builder error unchanged without retaining it.
    #[inline]
    pub fn initialize<E, F>(&mut self, build: F) -> Result<(), E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        if self.state.is_none() {
            self.initialization_attempts = self.initialization_attempts.saturating_add(1);
            self.state = Some(build()?);
        }
        Ok(())
    }

    /// Run setup until it succeeds, independently of application initialization.
    ///
    /// For example, install logging after its configuration becomes available.
    /// This guard belongs to this `Sandbox`, not the process. Callers must make
    /// failed setup safe to retry: partial side effects are not rolled back.
    ///
    /// # Errors
    /// Returns the setup error, leaving setup eligible for retry.
    #[inline]
    pub fn setup_once<E, F>(&mut self, setup: F) -> Result<(), E>
    where
        F: FnOnce() -> Result<(), E>,
    {
        if !self.setup_complete {
            setup()?;
            self.setup_complete = true;
        }
        Ok(())
    }

    #[cfg(any(feature = "fastly", test))]
    fn handle<Q, R, F>(&mut self, request: Q, handler: F) -> R
    where
        F: FnOnce(Q, &mut Self) -> R,
    {
        self.requests = self.requests.saturating_add(1);
        handler(request, self)
    }
}

/// Serve custom callbacks with lazy retained state and the supplied SDK limits.
///
/// The callback controls initialization, dispatch, finalization and streaming.
/// The callback owns sending and returns only a terminal `Result<(), E>`. The
/// SDK records that result without sending a synthetic error response. Errors
/// terminate serving, including failures after commitment, without a second send.
///
/// SDK limits are upper bounds, not a guarantee of reuse. This function does not
/// read configuration or enable reuse in existing entry points.
#[cfg(feature = "fastly")]
#[must_use = "inspect the serving summary or call into_result() to handle terminal errors"]
#[inline]
pub fn serve_custom<T, F, E>(serve: Serve, mut handler: F) -> ServeSummary<E>
where
    F: FnMut(fastly::Request, &mut Sandbox<T>) -> Result<(), E>,
{
    crate::init_fastly_abi();
    let mut sandbox = Sandbox::default();
    serve.run_with_context(
        |request, context: &mut Sandbox<T>| DeliveryResult(context.handle(request, &mut handler)),
        &mut sandbox,
    )
}

/// Handle one request with fresh state, without entering the SDK serving loop.
///
/// Takes a request already received by the caller. Sending remains owned by the
/// callback; this function never asks the SDK to render an error. Use an ordinary `main`.
///
/// # Errors
/// Returns the callback error unchanged without another response attempt.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_custom<T, E, F>(request: fastly::Request, handler: F) -> Result<(), E>
where
    F: FnOnce(fastly::Request, &mut Sandbox<T>) -> Result<(), E>,
{
    crate::init_fastly_abi();
    Sandbox::default().handle(request, handler)
}

#[cfg(test)]
mod tests {
    use super::{DeliveryResult, Sandbox};

    // Neither the retained value nor an error payload needs framework traits.
    struct App(u64);
    struct Fallback(&'static str);

    #[test]
    fn owned_delivery_retains_terminal_error_without_synthetic_send() {
        let result = DeliveryResult(Err::<(), _>("post-commit error"));
        assert_eq!(result.0, Err("post-commit error"));
    }

    #[cfg(feature = "fastly")]
    #[test]
    fn sdk_delivery_result_returns_terminal_error_without_synthetic_send() {
        use fastly::http::serve::HandlerResult as _;

        assert_eq!(
            DeliveryResult(Err::<(), _>("post-commit error")).send(),
            Err("post-commit error")
        );
        assert_eq!(DeliveryResult(Ok::<(), &str>(())).send(), Ok(()));
    }

    #[test]
    fn early_return_counts_request_without_setup_or_build() {
        let mut sandbox = Sandbox::<App>::default();
        sandbox.handle("health", |request, state| {
            assert_eq!(request, "health");
            assert_eq!(state.requests(), 1);
            assert_eq!(state.initialization_attempts(), 0);
            assert!(state.state().is_none());
            assert!(!state.setup_complete);
        });
    }

    #[test]
    fn failed_build_returns_payload_then_success_is_retained() {
        let mut sandbox = Sandbox::<App>::default();
        sandbox.handle("first", |_, state| {
            assert!(state.setup_once(|| Ok::<_, ()>(())).is_ok());
            let result = state.initialize(|| Err(Fallback("current request only")));
            assert!(matches!(result, Err(Fallback("current request only"))));
            assert!(state.state().is_none());
            assert_eq!(state.initialization_attempts(), 1);
        });
        for ordinal in 2..=4 {
            sandbox.handle(ordinal, |request, state| {
                assert!(
                    state
                        .setup_once::<(), _>(|| panic!("setup repeated"))
                        .is_ok()
                );
                assert!(
                    state
                        .initialize::<(), _>(|| {
                            assert_eq!(request, 2, "successful initialization repeated");
                            Ok(App(request))
                        })
                        .is_ok()
                );
                assert_eq!(state.state().unwrap().0, 2);
                assert_eq!(state.requests(), request);
                assert_eq!(state.initialization_attempts(), 2);
            });
        }
        let fresh = Sandbox::<App>::default();
        assert!(fresh.state().is_none());
        assert_eq!(fresh.requests(), 0);
        assert_eq!(fresh.initialization_attempts(), 0);
    }

    #[test]
    fn failed_setup_can_retry_without_initializing_the_app() {
        let mut sandbox = Sandbox::<App>::default();
        assert_eq!(sandbox.setup_once(|| Err("not ready")), Err("not ready"));
        assert!(!sandbox.setup_complete);
        assert!(sandbox.setup_once(|| Ok::<_, ()>(())).is_ok());
        assert!(
            sandbox
                .setup_once::<(), _>(|| panic!("setup repeated"))
                .is_ok()
        );
        assert_eq!(sandbox.initialization_attempts(), 0);
        assert_eq!(sandbox.requests(), 0);
    }
}
