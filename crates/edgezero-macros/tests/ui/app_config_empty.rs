//! An empty `#[app_config()]` must be a hard compile error, not a silent
//! no-op — otherwise the field would not be recursed and the child's
//! `#[secret]` metadata would be dropped without any diagnostic.

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[app_config()]
    child: String,
}

fn main() {}
