//! Pin test for the v1 canonical-form rules.
//!
//! The SHA below is committed verbatim.
//! `scripts/check_no_placeholder_pins.sh` (per spec section 13.1) which
//! refuses pin tests carrying unresolved placeholder hex; this file
//! ships the actual computed value so the gate stays green.

use edgezero_core::canonical_form::canonical_data_sha256;
use serde_json::json;

#[test]
#[expect(
    clippy::default_numeric_fallback,
    reason = "numeric literals in test fixture are intentional"
)]
#[expect(
    clippy::non_ascii_literal,
    reason = "UTF-8 fixture deliberately tests non-ASCII handling"
)]
#[expect(
    clippy::tests_outside_test_module,
    reason = "this is an integration test file in tests/"
)]
fn canonical_form_pin_v1() {
    // Fixture deliberately exercises every 4.2 escape-table branch:
    // - "héllo" → multi-byte UTF-8, no escape
    // - "tab\there" → \t (named escape)
    // - "quote\"backslash\\" → \" + \\ (named escapes)
    // - "newline\nhere" → \n (named escape)
    // - "carriage_return\r" → \r (named escape)
    // - "backspace\u{0008}" → \b (named escape)
    // - "formfeed\u{000C}" → \f (named escape)
    // - "\u{0001}" →  (generic control char branch)
    let data = json!({
        "greeting": "héllo",                  // verbatim UTF-8; NFC vs NFD hash differently per 4.2
        "tab": "tab\there",                   // \t named escape
        "quote_backslash": "quote\"backslash\\",  // \" + \\
        "newline": "newline\nhere",           // \n
        "carriage_return": "cr\rhere",        // \r named escape
        "backspace": "bs\u{0008}here",        // \b named escape
        "formfeed": "ff\u{000C}here",         // \f named escape
        "control_char": "\u{0001}",           //  generic-control branch
        "feature": { "new_checkout": true },
        "service": { "timeout_ms": 1500 },
        "ratio": 1.5,
        "missing": null,
        "empty": {}
    });
    let actual = canonical_data_sha256(&data);
    assert_eq!(
        actual,
        "903a0e4aa2e900e80ac64047fd6008a6e81c42d3cabd501ad7a22af06c2b2bbc"
    );
}
