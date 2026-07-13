//! `#[serde(rename = "...")]` on a `#[app_config(nested)]` field must
//! error — the emitter writes the Rust field name verbatim as a `Field`
//! path segment, which a rename would desync from the serialized key.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Child {
    #[secret]
    api_key: String,
}

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[serde(rename = "x")]
    #[app_config(nested)]
    child: Child,
}

fn main() {}
