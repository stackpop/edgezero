//! `#[secret]` is incompatible with `#[serde(flatten)]`.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[secret]
    #[serde(flatten)]
    api_token: String,
}

fn main() {}
