//! `#[app_config(bogus)]` must be a hard compile error (a typo must not
//! be silently ignored — that would drop the child's secrets).

#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
struct Config {
    #[app_config(bogus)]
    child: String,
}

fn main() {}
