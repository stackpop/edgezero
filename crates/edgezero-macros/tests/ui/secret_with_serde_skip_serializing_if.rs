//! `#[serde(skip_serializing_if = "...")]` conditionally omits the
//! field from serialisation. Combined with `#[secret]`, that would
//! make `config push` (which reads `secret_fields()`, then serialises
//! the typed struct) drop the secret key under the condition —
//! desyncing the on-the-wire shape from the secret_fields() invariant
//! relies on. Reject at compile time.

#[derive(serde::Deserialize, serde::Serialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
struct ConfigWithSkipSerializingIf {
    #[secret]
    #[serde(skip_serializing_if = "String::is_empty")]
    api_token: String,
}

fn main() {}
