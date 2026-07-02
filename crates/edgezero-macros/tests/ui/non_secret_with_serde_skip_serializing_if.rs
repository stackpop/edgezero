//! Spec 4.2 + 12.1: `#[serde(skip_serializing_if = "...")]` is
//! banned on EVERY field. Same reason as `skip_serializing`:
//! conditional omission lets the SHA-input shape depend on field
//! values, breaking the 4.2 stability contract.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    greeting: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    maybe: Option<String>,
}

fn main() {}
