#![cfg(not(target_arch = "wasm32"))]

extern crate self as spin_sdk;

mod variables {
    use std::fmt;

    use futures::future;

    #[derive(Debug)]
    pub(crate) enum Error {
        InvalidName(String),
        Other,
        Undefined(String),
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::InvalidName(message) => write!(f, "invalid name: {message}"),
                Self::Other => f.write_str("other"),
                Self::Undefined(message) => write!(f, "undefined: {message}"),
            }
        }
    }

    pub(crate) async fn get(_key: &str) -> Result<String, Error> {
        future::pending().await
    }
}

#[path = "../src/secret_store.rs"]
mod secret_store;

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::mem;
    use std::thread;
    use std::time::Duration;

    use bytes::Bytes;
    use edgezero_core::secret_store::SecretError;
    use edgezero_core::time::Deadline;
    use futures::executor::block_on;

    use super::{secret_store, variables};

    #[test]
    fn source_harness_constructs_spin_types() {
        let store = secret_store::SpinSecretStore::new();
        assert_eq!(mem::size_of_val(&store), 0);
        let errors = [
            variables::Error::Undefined(String::new()),
            variables::Error::InvalidName(String::new()),
            variables::Error::Other,
        ];
        for error in errors {
            match error {
                variables::Error::Undefined(message) | variables::Error::InvalidName(message) => {
                    assert!(message.is_empty());
                }
                variables::Error::Other => {}
            }
        }
    }

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
