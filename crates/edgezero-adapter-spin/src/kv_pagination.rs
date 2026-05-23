//! Pure paging logic for [`crate::SpinKvStore::list_keys_page`].
//!
//! Spin's `key_value::Store::get_keys()` returns **all** keys in the store
//! with no prefix, cursor, or limit support. The Spin adapter materialises
//! the full key list and pages it client-side here.
//!
//! Splitting this out from the wasm-only `SpinKvStore` lets the paging
//! invariants (prefix filtering, sort order, cursor advance, `max_list_keys`
//! cap) be unit-tested on the host without a Spin runtime.

use edgezero_core::key_value_store::{KvError, KvPage};

// The wasm32 `SpinKvStore` is the only production consumer; host builds only
// compile this module for its tests.
#[cfg_attr(
    not(any(test, all(feature = "spin", target_arch = "wasm32"))),
    expect(
        dead_code,
        reason = "wasm32-only consumer; host build compiles for tests"
    )
)]
/// Slice the result of `Store::get_keys()` into a single [`KvPage`].
///
/// - Filters by `prefix`; an empty prefix matches every key.
/// - Sorts matched keys lexicographically before paging.
/// - Returns [`KvError::LimitExceeded`] when the matched-key count exceeds
///   `max_list_keys`. `max_list_keys = 0` disables the cap.
/// - `cursor` is the last key of the previous page; only keys strictly greater
///   than it are emitted. The cursor is opaque to callers (the
///   [`crate::SpinKvStore`] wraps it in [`edgezero_core::key_value_store`]'s
///   prefix-stamped envelope at the trait boundary).
/// - The returned [`KvPage::cursor`] is the last key on this page when more
///   matches remain, `None` otherwise.
pub(crate) fn paginate_keys(
    all_keys: Vec<String>,
    prefix: &str,
    cursor: Option<&str>,
    limit: usize,
    max_list_keys: usize,
) -> Result<KvPage, KvError> {
    let mut matched: Vec<String> = if prefix.is_empty() {
        all_keys
    } else {
        all_keys
            .into_iter()
            .filter(|key| key.starts_with(prefix))
            .collect()
    };

    if max_list_keys > 0 && matched.len() > max_list_keys {
        return Err(KvError::LimitExceeded {
            message: format!(
                "{} keys match prefix {prefix:?}, exceeding max_list_keys={max_list_keys}",
                matched.len()
            ),
        });
    }

    matched.sort();

    let start_idx = match cursor {
        Some(after) => matched.partition_point(|key| key.as_str() <= after),
        None => 0,
    };
    let page_end = start_idx.saturating_add(limit).min(matched.len());
    let keys: Vec<String> = matched.get(start_idx..page_end).unwrap_or(&[]).to_vec();

    let has_more = page_end < matched.len();
    let next_cursor = if has_more { keys.last().cloned() } else { None };

    Ok(KvPage {
        keys,
        cursor: next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn empty_prefix_returns_all_sorted() {
        let page = paginate_keys(keys(&["b", "a", "c"]), "", None, 10, 100).expect("page");
        assert_eq!(page.keys, vec!["a", "b", "c"]);
        assert_eq!(page.cursor, None);
    }

    #[test]
    fn prefix_filters_then_sorts() {
        let page = paginate_keys(
            keys(&["user:42", "post:1", "user:1"]),
            "user:",
            None,
            10,
            100,
        )
        .expect("page");
        assert_eq!(page.keys, vec!["user:1", "user:42"]);
        assert_eq!(page.cursor, None);
    }

    #[test]
    fn page_smaller_than_match_yields_cursor() {
        let page = paginate_keys(keys(&["a", "b", "c", "d"]), "", None, 2, 100).expect("page");
        assert_eq!(page.keys, vec!["a", "b"]);
        assert_eq!(page.cursor.as_deref(), Some("b"));
    }

    #[test]
    fn cursor_advances_past_previous_page() {
        let page = paginate_keys(keys(&["a", "b", "c", "d"]), "", Some("b"), 2, 100).expect("page");
        assert_eq!(page.keys, vec!["c", "d"]);
        assert_eq!(page.cursor, None);
    }

    #[test]
    fn final_page_returns_no_cursor() {
        let page = paginate_keys(keys(&["a", "b"]), "", None, 10, 100).expect("page");
        assert_eq!(page.keys, vec!["a", "b"]);
        assert_eq!(page.cursor, None);
    }

    #[test]
    fn cap_exceeded_returns_limit_exceeded() {
        let err = paginate_keys(keys(&["a", "b", "c", "d"]), "", None, 10, 2)
            .expect_err("expected LimitExceeded");
        if let KvError::LimitExceeded { message } = err {
            assert!(message.contains("max_list_keys=2"));
            assert!(message.contains("4 keys"));
        } else {
            panic!("expected LimitExceeded, got {err:?}");
        }
    }

    #[test]
    fn cap_zero_disables_check() {
        let page = paginate_keys(keys(&["a", "b", "c"]), "", None, 10, 0).expect("page");
        assert_eq!(page.keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn cap_applies_after_prefix_filter() {
        // 3 matching keys, cap=2 → exceeded
        let err = paginate_keys(
            keys(&["user:1", "user:2", "user:3", "post:99"]),
            "user:",
            None,
            10,
            2,
        )
        .expect_err("expected LimitExceeded");
        assert!(matches!(err, KvError::LimitExceeded { .. }));

        // Same data, prefix that matches only 1 → under cap
        let page = paginate_keys(keys(&["user:1", "post:1", "post:2"]), "post:", None, 10, 2)
            .expect("page");
        assert_eq!(page.keys, vec!["post:1", "post:2"]);
    }

    #[test]
    fn cursor_past_last_key_yields_empty_page() {
        let page = paginate_keys(keys(&["a", "b"]), "", Some("zzz"), 10, 100).expect("page");
        assert!(page.keys.is_empty());
        assert_eq!(page.cursor, None);
    }
}
