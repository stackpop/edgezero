//! Spin adapter secret store: wraps `spin_sdk::variables`.
//!
//! Spin's variable namespace is flat — there is no concept of named stores.
//! The `store_name` parameter is intentionally ignored; provision secrets as
//! application variables in `spin.toml`.

use std::future::Future;
#[cfg(test)]
use std::future::{pending, ready};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::config_store::BoundedStoreRead;
use edgezero_core::secret_store::{SecretError, SecretStore, finish_bounded_secret_read};
use edgezero_core::time::{Deadline, MonotonicClock};
use futures_util::future::{Either, select};
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use spin_sdk::time::sleep;

/// Secret store backed by Spin component variables.
///
/// `store_name` is ignored — Spin's variable namespace is flat.
/// Provision secrets as application variables in `spin.toml`.
pub struct SpinSecretStore;

impl SpinSecretStore {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for SpinSecretStore {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl SecretStore for SpinSecretStore {
    #[inline]
    async fn get_bytes(&self, store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError> {
        use spin_sdk::variables;
        if !store_name.is_empty() {
            // Spin's variable namespace is flat; named stores are not supported.
            log::debug!(
                "SpinSecretStore: store_name {store_name:?} is ignored; \
                 Spin uses a single flat variable namespace"
            );
        }
        // Spin variable names must be lowercase. Normalise via ascii_lowercase
        // so that SCREAMING_SNAKE_CASE keys (e.g. "STRIPE_KEY" → "stripe_key")
        // work without callers knowing the Spin convention. Note: only
        // UPPER_SNAKE → lower_snake is safe; camelCase or mixed-case keys will
        // be lowercased in a way that may not match any declared variable
        // (e.g. "stripeKey" → "stripekey"). Document accepted key formats at
        // the call site.
        let lower = key.to_ascii_lowercase();
        match variables::get(&lower).await {
            Ok(value) => Ok(Some(Bytes::from(value.into_bytes()))),
            Err(variables::Error::Undefined(_)) => Ok(None),
            Err(variables::Error::InvalidName(msg)) => Err(SecretError::Validation(msg)),
            Err(err) => Err(SecretError::Internal(anyhow::anyhow!(
                "secret lookup failed: {err}"
            ))),
        }
    }

    #[inline]
    async fn get_bytes_bounded(
        &self,
        store_name: &str,
        key: &str,
        clock: &MonotonicClock,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<Bytes>, SecretError> {
        bounded_secret_read(
            self.get_bytes(store_name, key),
            clock,
            deadline,
            max_backend_bytes,
            max_value_bytes,
        )
        .await
    }
}

// Spin Variables returns a complete value, so the byte bounds apply after host
// materialization. The timer race bounds guest return but does not prove host cancellation.
pub(crate) async fn bounded_secret_read<F>(
    read: F,
    clock: &MonotonicClock,
    deadline: Deadline,
    max_backend_bytes: u64,
    max_value_bytes: u64,
) -> Result<BoundedStoreRead<Bytes>, SecretError>
where
    F: Future<Output = Result<Option<Bytes>, SecretError>>,
{
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    {
        return bounded_secret_read_with_timer(
            read,
            clock,
            deadline,
            max_backend_bytes,
            max_value_bytes,
            sleep,
        )
        .await;
    }
    #[cfg(not(all(feature = "spin", target_arch = "wasm32")))]
    {
        bounded_secret_read_with_timer(
            read,
            clock,
            deadline,
            max_backend_bytes,
            max_value_bytes,
            |_| pending(),
        )
        .await
    }
}

async fn bounded_secret_read_with_timer<F, MakeTimer, Timer>(
    read: F,
    clock: &MonotonicClock,
    deadline: Deadline,
    max_backend_bytes: u64,
    max_value_bytes: u64,
    mut make_timer: MakeTimer,
) -> Result<BoundedStoreRead<Bytes>, SecretError>
where
    F: Future<Output = Result<Option<Bytes>, SecretError>>,
    MakeTimer: FnMut(Duration) -> Timer,
    Timer: Future<Output = ()>,
{
    futures_util::pin_mut!(read);
    loop {
        let Some(remaining) = deadline.remaining_at(clock.now()) else {
            return Err(SecretError::DeadlineExceeded);
        };
        let timer = make_timer(remaining);
        futures_util::pin_mut!(timer);
        match select(timer, read.as_mut()).await {
            Either::Left(((), _read)) => {
                if deadline.is_expired_at(clock.now()) {
                    return Err(SecretError::DeadlineExceeded);
                }
            }
            Either::Right((result, _timer)) => {
                return finish_bounded_secret_read(
                    result,
                    clock,
                    deadline,
                    max_backend_bytes,
                    max_value_bytes,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use edgezero_core::time::MonotonicClock;
    use futures::executor::block_on;

    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn clock_reaching_deadline() -> (MonotonicClock, Deadline) {
        let start = edgezero_core::MonotonicInstant::now();
        let terminal = start
            .checked_add(Duration::from_secs(1))
            .expect("terminal instant");
        let samples = Arc::new(AtomicUsize::new(0));
        let observed_samples = Arc::clone(&samples);
        let clock = MonotonicClock::new(move || {
            if observed_samples.fetch_add(1, Ordering::SeqCst) == 0 {
                start
            } else {
                terminal
            }
        });
        (clock, Deadline::at_instant(terminal))
    }

    #[test]
    fn bounded_secret_timer_drops_never_ready_provider() {
        let drops = Arc::new(AtomicUsize::new(0));
        let guard = DropProbe(Arc::clone(&drops));
        let read = async move {
            let _guard = guard;
            pending::<Result<Option<Bytes>, SecretError>>().await
        };
        let (clock, deadline) = clock_reaching_deadline();

        let error = block_on(bounded_secret_read_with_timer(
            read,
            &clock,
            deadline,
            1,
            1,
            |_| ready(()),
        ))
        .expect_err("timer must terminate a pending secret read");

        assert!(matches!(error, SecretError::DeadlineExceeded));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bounded_secret_read_rearms_after_an_early_timer_wake() {
        let timer_calls = Arc::new(AtomicUsize::new(0));
        let observed_timer_calls = Arc::clone(&timer_calls);
        let clock = MonotonicClock::default();
        let error = block_on(bounded_secret_read_with_timer(
            ready(Err(SecretError::Unavailable)),
            &clock,
            Deadline::after(Duration::from_secs(1)),
            1,
            1,
            move |_| {
                let early_wake = observed_timer_calls.fetch_add(1, Ordering::SeqCst) == 0;
                async move {
                    if !early_wake {
                        pending::<()>().await;
                    }
                }
            },
        ))
        .expect_err("provider error must win after an early timer wake");

        assert!(matches!(error, SecretError::Unavailable));
        assert_eq!(timer_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn bounded_secret_timer_wins_simultaneous_ready_error_at_deadline() {
        let (clock, deadline) = clock_reaching_deadline();
        let error = block_on(bounded_secret_read_with_timer(
            ready(Err(SecretError::Unavailable)),
            &clock,
            deadline,
            1,
            1,
            |_| ready(()),
        ))
        .expect_err("timer readiness at the deadline must win equality");

        assert!(matches!(error, SecretError::DeadlineExceeded));
    }
}
