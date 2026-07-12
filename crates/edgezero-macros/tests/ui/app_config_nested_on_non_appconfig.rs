//! `#[app_config(nested)]` on a field whose type does not derive
//! `AppConfig` must fail with a clear `AppConfigRoot` bound error.

#[derive(serde::Deserialize)]
struct NotAppConfig {
    _key: String,
}

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[app_config(nested)]
    child: NotAppConfig,
}

fn main() {}
