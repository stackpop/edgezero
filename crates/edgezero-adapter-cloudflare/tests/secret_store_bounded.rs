#[path = "../src/secret_store.rs"]
mod secret_store;

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::thread;
    use std::time::Duration;

    use bytes::Bytes;
    use edgezero_core::secret_store::SecretError;
    use edgezero_core::time::Deadline;
    use futures::executor::block_on;

    use super::secret_store;

    #[test]
    fn bounded_secret_reports_exact_bytes_and_accepts_exact_caps() {
        let result = block_on(secret_store::bounded_secret_read(
            async { Ok(Some(Bytes::from_static(b"value"))) },
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
            let error = block_on(secret_store::bounded_secret_read(
                async { Ok(Some(Bytes::from_static(b"value"))) },
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
        let error = block_on(secret_store::bounded_secret_read(
            async {
                polled.set(true);
                Ok(None)
            },
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
        let error = block_on(secret_store::bounded_secret_read(
            async {
                thread::sleep(Duration::from_millis(10));
                Ok(None)
            },
            Deadline::after(Duration::from_millis(1)),
            1,
            1,
        ))
        .expect_err("a host call completing after the deadline must fail");

        assert!(matches!(error, SecretError::DeadlineExceeded));
    }

    #[test]
    fn bounded_secret_deadline_wins_over_late_host_error() {
        let error = block_on(secret_store::bounded_secret_read(
            async {
                thread::sleep(Duration::from_millis(10));
                Err(SecretError::Unavailable)
            },
            Deadline::after(Duration::from_millis(1)),
            1,
            1,
        ))
        .expect_err("the post-call deadline check must run after host errors");

        assert!(matches!(error, SecretError::DeadlineExceeded));
    }
}
