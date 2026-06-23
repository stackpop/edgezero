//! Spec 4.2 + 12.1: `#[serde(flatten)]` is banned on EVERY field
//! (not just `#[secret]` ones) because the canonical form must
//! deterministically project struct fields to JSON object keys.
//! Flatten makes the canonical SHA dependent on the contents of an
//! arbitrary inner type, which breaks the 4.2 stability contract.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[serde(flatten)]
    inner: serde_json::Value,
}

fn main() {}
