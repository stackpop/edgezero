//! `#[secret(store_ref = "vault")]` names a sibling field `vault` that exists
//! but is NOT annotated with `#[secret(store_ref)]`. The derive must reject
//! this at compile time.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
pub struct C {
    #[secret(store_ref = "vault")]
    api_token: String,
    // `vault` exists but has no `#[secret(store_ref)]` annotation.
    vault: String,
}

fn main() {}
