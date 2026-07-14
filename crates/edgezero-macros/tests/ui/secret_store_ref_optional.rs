//! `#[secret(store_ref)]` on `Option<String>` must error — a store id is
//! structural and must always be present.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[secret(store_ref)]
    vault: Option<String>,
}

fn main() {}
