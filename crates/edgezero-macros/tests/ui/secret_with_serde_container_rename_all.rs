//! Container-level `#[serde(rename_all = ...)]` on a struct that has a
//! `#[secret]` field must be rejected: the renamer would translate the
//! TOML key to `api-token` while `secret_fields()` keeps reporting
//! `api_token`, silently desyncing the typed `config validate` secret
//! checks and the Spin collision check.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(rename_all = "kebab-case")]
struct ConfigWithRenameAll {
    #[secret]
    api_token: String,
}

fn main() {}
