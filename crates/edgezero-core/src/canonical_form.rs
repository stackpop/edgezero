//! Canonical-form SHA-256 over a [`serde_json::Value`] tree.
//!
//! Implements the v1 rules from
//! `docs/superpowers/specs/2026-06-16-blob-app-config.md` 4.2:
//!
//! - JSON with no insignificant whitespace.
//! - Object keys sorted by UTF-8 byte order.
//! - Strings emitted verbatim (no NFC fold).
//! - Floats rendered via `ryu`'s shortest round-trippable form.
//!   Non-finite floats are rejected upstream (loader / env overlay);
//!   if one reaches this walker, it panics on the assertion that
//!   guards against silent JSON-`null` collapse.
//! - Booleans `true` / `false` lowercase; `null` literal; `{}` and
//!   `[]` for empties.
//! - UTF-8 bytes of the resulting string feed into `Sha256`; hex
//!   output is lowercase, no `0x` prefix.

use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;

/// SHA-256 of the canonical form of `data`. See module docs.
#[must_use]
#[inline]
pub fn canonical_data_sha256(data: &Value) -> String {
    #[cfg(test)]
    test_hooks::CALL_COUNT.with(|cell| cell.set(cell.get().saturating_add(1)));
    let mut buf = String::new();
    write_canonical(&mut buf, data);
    let mut hasher = Sha256::new();
    hasher.update(buf.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
pub(crate) mod test_hooks {
    //! Test-only per-thread invocation counter so other modules' tests
    //! can pin the spec 12.1 "canonicaliser MUST NOT run on this input"
    //! property for non-finite-rejected fixtures. Thread-local (not a
    //! global atomic) so parallel test execution does not race — each
    //! test sees only the calls made on its own thread.
    use std::cell::Cell;
    thread_local! {
        pub(crate) static CALL_COUNT: Cell<usize> = const { Cell::new(0) };
    }
}

fn write_canonical(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(out, n),
        Value::String(str_val) => write_string(out, str_val),
        Value::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Sort keys by UTF-8 byte order. `BTreeMap` would also work,
            // but `serde_json::Map` may preserve insertion order under
            // its default feature set, so we copy into a Vec and sort.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|lhs, rhs| lhs.as_bytes().cmp(rhs.as_bytes()));
            out.push('{');
            for (idx, key) in keys.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_canonical(out, &map[*key]);
            }
            out.push('}');
        }
    }
}

fn write_number(out: &mut String, n: &serde_json::Number) {
    if let Some(int_val) = n.as_i64() {
        // Infallible: writing to a String never fails.
        write!(out, "{int_val}").unwrap_or_default();
    } else if let Some(uint_val) = n.as_u64() {
        write!(out, "{uint_val}").unwrap_or_default();
    } else {
        // f64 branch. serde_json::Number without the arbitrary_precision
        // feature always exposes i64/u64/f64; both preceding checks
        // failed so `.as_f64()` is guaranteed Some. Fall back to NaN
        // when the impossible-fourth-shape case ever surfaces — the
        // is_finite() assert below will fire the same message either
        // way, so we lose no diagnostic signal by collapsing the two
        // panics into one assert.
        //
        // Non-finite floats are loader-rejected per 4.2. If one reaches
        // here, it's a programmer error: serialising NaN would emit
        // `null` via serde_json's default, which would collide with
        // real Option::None in the SHA.
        let float_val = n.as_f64().unwrap_or(f64::NAN);
        assert!(
            float_val.is_finite(),
            "canonical_data_sha256: non-finite float {float_val} reached canonicaliser; loader must reject before this point"
        );
        let mut ryu_buf = ryu::Buffer::new();
        out.push_str(ryu_buf.format(float_val));
    }
}

fn write_string(out: &mut String, raw: &str) {
    // Escape table byte-identical to serde_json::to_string(raw)'s
    // default. Pinned by spec 4.2 — DO NOT change without bumping
    // BlobEnvelope::version.
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            ctrl if u32::from(ctrl) < 0x20 => {
                // Lowercase hex, 4 digits, zero-padded. Matches
                // serde_json's `\u00XX` emission for control chars
                // outside the named-escape set.
                write!(out, "\\u{:04x}", u32::from(ctrl)).unwrap_or_default();
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_keys_sorted_by_utf8_bytes() {
        let val_ba = json!({ "b": 1_i32, "a": 2_i32 });
        let val_ab = json!({ "a": 2_i32, "b": 1_i32 });
        assert_eq!(
            canonical_data_sha256(&val_ba),
            canonical_data_sha256(&val_ab)
        );
    }

    #[test]
    fn finite_floats_round_trip_via_ryu() {
        // 1.5, 1.5000000, 15e-1 all parse to the same f64 and must
        // produce the same sha.
        let val_a: Value = serde_json::from_str("{\"x\": 1.5}").unwrap();
        let val_b: Value = serde_json::from_str("{\"x\": 1.5000000}").unwrap();
        let val_c: Value = serde_json::from_str("{\"x\": 15e-1}").unwrap();
        assert_eq!(canonical_data_sha256(&val_a), canonical_data_sha256(&val_b));
        assert_eq!(canonical_data_sha256(&val_a), canonical_data_sha256(&val_c));
    }

    #[test]
    fn empty_object_and_empty_array_distinct_from_null() {
        let hash_null = canonical_data_sha256(&json!({ "x": null }));
        let hash_obj = canonical_data_sha256(&json!({ "x": {} }));
        let hash_arr = canonical_data_sha256(&json!({ "x": [] }));
        assert_ne!(hash_null, hash_obj);
        assert_ne!(hash_null, hash_arr);
        assert_ne!(hash_obj, hash_arr);
    }

    #[test]
    fn integer_and_float_with_same_text_hash_differently() {
        // 4.2 type-identity rule. 1500 vs 1500.0.
        let val_int = json!({ "x": 1500_i64 });
        let val_flt = json!({ "x": 1500.0_f64 });
        assert_ne!(
            canonical_data_sha256(&val_int),
            canonical_data_sha256(&val_flt)
        );
    }

    // Note: the loader rejects non-finite floats before they
    // reach this walker. The `assert!(f.is_finite(), ...)` in
    // `write_number` is a programmer-error guard; the test for the
    // loader rejection lives in `app_config::tests`.
}
