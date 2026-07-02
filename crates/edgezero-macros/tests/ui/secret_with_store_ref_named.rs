//! Happy path: `#[secret(store_ref = "vault")]` with a valid `#[secret(store_ref)]`
//! sibling field. The derive must accept this without error.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
pub struct C {
    #[secret(store_ref = "vault")]
    api_token: String,
    #[secret(store_ref)]
    vault: String,
}

fn main() {}
