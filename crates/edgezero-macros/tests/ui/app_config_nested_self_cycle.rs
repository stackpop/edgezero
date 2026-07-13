//! `#[app_config(nested)]` on a field whose type is the enclosing struct
//! (directly or through `Vec`) is a compile error: the generated
//! `secret_fields()` would recurse into itself forever.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[app_config(nested)]
    children: Vec<Config>,
}

fn main() {}
