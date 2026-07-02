//! `#[secret(store_ref = "vault")]` names a sibling field `vault` that does
//! not exist on the struct. The derive must reject this at compile time.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
pub struct C {
    #[secret(store_ref = "vault")]
    api_token: String,
    // `vault` field is absent — the macro must error.
}

fn main() {}
