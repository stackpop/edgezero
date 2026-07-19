//! Versioned envelope wrapping the canonical-form `data` blob.
//!
//! Shape per spec 4.1:
//! ```json
//! { "data": {...}, "sha256": "<hex>", "version": 1, "generated_at": "<RFC3339 UTC>" }
//! ```

use core::fmt;

use crate::canonical_form::canonical_data_sha256;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current envelope version. Bumped only when the canonical-form
/// rules change in a way that breaks pin compatibility.
pub const ENVELOPE_VERSION_V1: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BlobEnvelope {
    pub data: serde_json::Value,
    pub generated_at: String,
    pub sha256: String,
    pub version: u32,
}

#[derive(Error)]
pub enum BlobEnvelopeError {
    // The stored/computed hashes are NOT interpolated into the Display message.
    // `stored` is a blob-controlled string (a malformed envelope can set it to
    // anything, including a secret), and this error is formatted into diagnostics
    // that reach HTTP responses and logs. Redacting at the source covers every
    // caller — the extractor, the chunk resolver, the CLI push/diff paths, and
    // the introspection endpoint — instead of each having to remember to. The
    // fields stay on the struct for programmatic inspection.
    #[error("stored SHA-256 does not match the computed hash (hashes redacted)")]
    ShaMismatch { stored: String, computed: String },
    #[error("unknown envelope version {0}; expected {expected}", expected = ENVELOPE_VERSION_V1)]
    UnknownVersion(u32),
}

// Debug is hand-written (NOT derived): a derived `Debug` would print the
// `stored`/`computed` hashes, so `{err:?}` / `?err` (anyhow) would leak what the
// Display message deliberately redacts. Debug therefore mirrors Display.
impl fmt::Debug for BlobEnvelopeError {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShaMismatch { .. } => f.write_str("ShaMismatch { hashes redacted }"),
            Self::UnknownVersion(version) => write!(f, "UnknownVersion({version})"),
        }
    }
}

impl BlobEnvelope {
    /// Consume the envelope and return only the inner `data` value.
    #[must_use]
    #[inline]
    pub fn into_data(self) -> serde_json::Value {
        self.data
    }

    /// Construct an envelope from `data` by computing the canonical SHA
    /// and stamping `version`. `generated_at` is passed in by the
    /// caller (we don't take a clock dep here).
    #[must_use]
    #[inline]
    pub fn new(data: serde_json::Value, generated_at: String) -> Self {
        let sha256 = canonical_data_sha256(&data);
        Self {
            data,
            generated_at,
            sha256,
            version: ENVELOPE_VERSION_V1,
        }
    }

    /// Verify the envelope's `version` and recomputed sha against
    /// the embedded `sha256`. Returns `Ok(())` on agreement;
    /// `Err(BlobEnvelopeError)` otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`BlobEnvelopeError::UnknownVersion`] if `version != ENVELOPE_VERSION_V1`.
    /// Returns [`BlobEnvelopeError::ShaMismatch`] if the recomputed SHA differs from `sha256`.
    #[must_use = "discarding the error silently defeats integrity checking"]
    #[inline]
    pub fn verify(&self) -> Result<(), BlobEnvelopeError> {
        if self.version != ENVELOPE_VERSION_V1 {
            return Err(BlobEnvelopeError::UnknownVersion(self.version));
        }
        let computed = canonical_data_sha256(&self.data);
        if computed != self.sha256 {
            return Err(BlobEnvelopeError::ShaMismatch {
                stored: self.sha256.clone(),
                computed,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_then_verify_round_trips() {
        let envelope =
            BlobEnvelope::new(json!({ "greeting": "hi" }), "2026-06-17T00:00:00Z".into());
        envelope.verify().unwrap();
    }

    #[test]
    fn sha_mismatch_display_does_not_leak_the_stored_hash() {
        // The stored SHA is blob-controlled and this Display is formatted into
        // diagnostics that reach HTTP responses and logs by many callers.
        // Redacting here is the single point that covers all of them.
        const SENTINEL: &str = "SUPER_SECRET_STORED_HASH";
        let mut envelope = BlobEnvelope::new(json!({ "greeting": "hi" }), "2026-06-17".into());
        envelope.sha256 = SENTINEL.to_owned();
        let err = envelope
            .verify()
            .expect_err("a tampered sha must fail verify");
        assert!(
            !err.to_string().contains(SENTINEL),
            "the stored hash must never appear in the error Display: {err}"
        );
        // Debug must redact too: `{err:?}` / `?err` (anyhow) would otherwise
        // print the stored hash even though Display redacts it.
        assert!(
            !format!("{err:?}").contains(SENTINEL),
            "the stored hash must never appear in the error Debug: {err:?}"
        );
    }

    #[test]
    fn rejects_unknown_version() {
        let mut envelope = BlobEnvelope::new(json!({ "x": 1_i32 }), "2026-06-17T00:00:00Z".into());
        envelope.version = 99;
        assert!(matches!(
            envelope.verify(),
            Err(BlobEnvelopeError::UnknownVersion(99))
        ));
    }

    #[test]
    fn detects_sha_mismatch() {
        let mut envelope = BlobEnvelope::new(json!({ "x": 1_i32 }), "2026-06-17T00:00:00Z".into());
        envelope.sha256 = "ff".repeat(32);
        assert!(matches!(
            envelope.verify(),
            Err(BlobEnvelopeError::ShaMismatch { .. })
        ));
    }

    #[test]
    fn json_round_trip_preserves_fields() {
        let envelope = BlobEnvelope::new(json!({ "x": 1_i32 }), "2026-06-17T00:00:00Z".into());
        let json_str = serde_json::to_string(&envelope).unwrap();
        let parsed: BlobEnvelope = serde_json::from_str(&json_str).unwrap();
        parsed.verify().unwrap();
    }

    #[test]
    fn tolerates_additional_ignored_fields() {
        // Spec 12.1 forward-compat: an envelope carrying extra fields
        // (e.g., a future `signature` envelope add-on) deserialises
        // cleanly without `deny_unknown_fields`. The known fields verify
        // unchanged; the extras are dropped.
        let envelope = BlobEnvelope::new(json!({ "x": 1_i32 }), "2026-06-17T00:00:00Z".into());
        let with_extras = json!({
            "data": envelope.data,
            "sha256": envelope.sha256,
            "version": envelope.version,
            "generated_at": envelope.generated_at,
            "signature": "future-field-the-runtime-ignores",
            "extra_metadata": { "tool": "v2-author" },
        });
        let parsed: BlobEnvelope = serde_json::from_value(with_extras).unwrap();
        parsed.verify().unwrap();
    }
}
