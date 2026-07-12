//! A parent with only `#[app_config(nested)]` children (no direct
//! `#[secret]`) carrying `#[serde(rename_all = ...)]` must error — the
//! rename would desync the emitted `Field(parent_field)` path segment.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Child {
    #[secret]
    api_key: String,
}

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(rename_all = "kebab-case")]
struct Config {
    #[app_config(nested)]
    child_config: Child,
}

fn main() {}
