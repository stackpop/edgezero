//! Private snapshot read contracts, with no service client or credentials.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use edgezero_core::secret_store::{SecretError, SecretStore as _};
use edgezero_core::secret_store_contract_tests;
use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
use futures::executor::block_on;

use crate::preparation::SecretSnapshot;

fn snapshot() -> SecretSnapshot {
    SecretSnapshot {
        namespace: "mystore".to_owned(),
        entries: BTreeMap::from([
            (
                "contract_key".to_owned(),
                Bytes::from_static(b"contract_value"),
            ),
            (
                "contract_key_2".to_owned(),
                Bytes::from_static(b"another_value"),
            ),
        ]),
    }
}

#[test]
fn final_deadline_check_precedes_size_error() {
    let start = MonotonicInstant::now();
    let sampled = Arc::new(Mutex::new(start));
    let clock = MonotonicClock::new(move || {
        let mut now = sampled.lock().expect("clock mutex");
        let result = *now;
        *now = now
            .checked_add(Duration::from_millis(1))
            .expect("clock advance");
        result
    });
    let deadline = Deadline::at_instant(
        start
            .checked_add(Duration::from_millis(2))
            .expect("deadline"),
    );
    let result =
        block_on(snapshot().get_bytes_bounded("mystore", "contract_key", &clock, deadline, 1, 1));
    assert!(matches!(result, Err(SecretError::DeadlineExceeded)));
}

#[test]
fn namespace_miss_and_backend_and_value_caps_are_enforced() {
    let clock = MonotonicClock::default();
    let deadline = Deadline::at_instant(
        clock
            .now()
            .checked_add(Duration::from_secs(1))
            .expect("deadline"),
    );
    let provider = snapshot();
    let miss =
        block_on(provider.get_bytes_bounded("other", "contract_key", &clock, deadline, 0, 0))
            .expect("namespace miss");
    assert!(miss.value.is_none());
    for (backend_cap, value_cap) in [(1_u64, 100_u64), (100_u64, 1_u64)] {
        let result = block_on(provider.get_bytes_bounded(
            "mystore",
            "contract_key",
            &clock,
            deadline,
            backend_cap,
            value_cap,
        ));
        assert!(matches!(result, Err(SecretError::ValueTooLarge)));
    }
}

secret_store_contract_tests!(secret_snapshot_contract, snapshot());
