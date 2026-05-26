//! `#[secret(...)]` accepts only `store_ref`; any other argument is a
//! compile error.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[secret(bogus)]
    api_token: String,
}

fn main() {}
