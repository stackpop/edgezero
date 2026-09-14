#![cfg(all(feature = "cloudflare", not(target_arch = "wasm32")))]

#[path = "../src/secret_store.rs"]
#[expect(
    dead_code,
    reason = "the source harness exercises the private bounded-read helper without constructing a worker Env"
)]
mod secret_store;

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use bytes::Bytes;
    use edgezero_core::secret_store::SecretError;
    use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
    use futures::executor::block_on;

    use super::secret_store;

    #[test]
    fn bounded_secret_reports_exact_bytes_and_accepts_exact_caps() {
        let clock = MonotonicClock::default();
        let result = block_on(secret_store::bounded_secret_read(
            async { Ok(Some(Bytes::from_static(b"value"))) },
            &clock,
            Deadline::after(Duration::from_secs(1)),
            5,
            5,
        ))
        .expect("exact caps must succeed");

        assert_eq!(result.backend_bytes, 5);
        assert_eq!(result.value, Some(Bytes::from_static(b"value")));
    }

    #[test]
    fn bounded_secret_rejects_either_exceeded_cap() {
        for (max_backend_bytes, max_value_bytes) in [(4, 5), (5, 4)] {
            let clock = MonotonicClock::default();
            let error = block_on(secret_store::bounded_secret_read(
                async { Ok(Some(Bytes::from_static(b"value"))) },
                &clock,
                Deadline::after(Duration::from_secs(1)),
                max_backend_bytes,
                max_value_bytes,
            ))
            .expect_err("an exceeded cap must fail");

            assert!(matches!(error, SecretError::ValueTooLarge));
        }
    }

    #[test]
    fn bounded_secret_checks_deadline_before_polling_host_call() {
        let polled = Cell::new(false);
        let clock = MonotonicClock::default();
        let error = block_on(secret_store::bounded_secret_read(
            async {
                polled.set(true);
                Ok(None)
            },
            &clock,
            Deadline::after(Duration::ZERO),
            1,
            1,
        ))
        .expect_err("expired deadline must fail");

        assert!(matches!(error, SecretError::DeadlineExceeded));
        assert!(!polled.get(), "expired reads must not poll the host call");
    }

    #[test]
    fn bounded_secret_checks_deadline_after_host_call() {
        let start = MonotonicInstant::now();
        let terminal = start
            .checked_add(Duration::from_secs(1))
            .expect("terminal instant");
        let now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&now);
        let clock = MonotonicClock::new(move || *clock_now.lock().expect("clock lock"));
        let error = block_on(secret_store::bounded_secret_read(
            async {
                *now.lock().expect("clock lock") = terminal;
                Ok(None)
            },
            &clock,
            Deadline::at_instant(terminal),
            1,
            1,
        ))
        .expect_err("a host call completing after the deadline must fail");

        assert!(matches!(error, SecretError::DeadlineExceeded));
    }

    #[test]
    fn bounded_secret_deadline_wins_over_late_host_error() {
        let start = MonotonicInstant::now();
        let terminal = start
            .checked_add(Duration::from_secs(1))
            .expect("terminal instant");
        let now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&now);
        let clock = MonotonicClock::new(move || *clock_now.lock().expect("clock lock"));
        let error = block_on(secret_store::bounded_secret_read(
            async {
                *now.lock().expect("clock lock") = terminal;
                Err(SecretError::Unavailable)
            },
            &clock,
            Deadline::at_instant(terminal),
            1,
            1,
        ))
        .expect_err("the post-call deadline check must run after host errors");

        assert!(matches!(error, SecretError::DeadlineExceeded));
    }

    #[test]
    fn bounded_secret_read_uses_injected_clock() {
        let process_now = MonotonicInstant::now();
        let injected_now = process_now
            .checked_sub(Duration::from_mins(1))
            .expect("injected instant");
        let clock = MonotonicClock::new(move || injected_now);
        let deadline = Deadline::at_instant(
            injected_now
                .checked_add(Duration::from_secs(1))
                .expect("deadline instant"),
        );

        let result = block_on(secret_store::bounded_secret_read(
            async { Ok(Some(Bytes::from_static(b"value"))) },
            &clock,
            deadline,
            5,
            5,
        ))
        .expect("the application clock is still before its deadline");

        assert_eq!(result.value, Some(Bytes::from_static(b"value")));
    }
}
