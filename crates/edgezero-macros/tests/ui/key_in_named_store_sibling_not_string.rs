//! `#[secret(store_ref = "vault")]` names a sibling field `vault` that is
//! annotated `#[secret(store_ref)]` but has type `u32` instead of `String`.
//! The derive must reject the non-String type at compile time.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
pub struct C {
    #[secret(store_ref = "vault")]
    api_token: String,
    // `vault` is `#[secret(store_ref)]` but has the wrong type.
    #[secret(store_ref)]
    vault: u32,
}

fn main() {}
