//! `#[secret]` on `Option<u32>` must error — only `String` or
//! `Option<String>` are accepted.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[secret]
    api_token: Option<u32>,
}

fn main() {}
