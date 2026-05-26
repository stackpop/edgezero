//! `#[secret]` must annotate a scalar string field; a non-scalar type
//! (e.g. `Vec<String>`) is a compile error.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[secret]
    api_tokens: Vec<String>,
}

fn main() {}
