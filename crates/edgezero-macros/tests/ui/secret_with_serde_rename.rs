//! `#[secret]` is incompatible with `#[serde(rename)]`.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[secret]
    #[serde(rename = "token")]
    api_token: String,
}

fn main() {}
