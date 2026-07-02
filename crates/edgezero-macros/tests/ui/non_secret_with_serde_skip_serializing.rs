//! Spec 4.2 + 12.1: `#[serde(skip_serializing)]` is banned on
//! EVERY field because it makes the canonical form omit the field
//! (and therefore omit it from the SHA), while runtime deserialise
//! would expect it. Push and runtime would disagree silently.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    greeting: String,
    #[serde(skip_serializing)]
    omitted: String,
}

fn main() {}
