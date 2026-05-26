//! Typed application config for `app-demo`, loaded from `app-demo.toml`
//! via `edgezero_core::app_config::load_app_config::<AppDemoConfig>`.
//!
//! The `app_demo__<SECTION>__…__<KEY>` env-var overlay (uppercase,
//! `-`→`_`) overrides any key already present in the file.

#![expect(
    clippy::module_name_repetitions,
    reason = "`<Name>Config` is the canonical name the generator emits and the spec refers to (§6.8)"
)]

use serde::{Deserialize, Serialize};
use validator::Validate;

#[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
pub struct AppDemoConfig {
    /// Resolved at runtime via
    /// `ctx.secret_store_default()?.require_str(&cfg.api_token)`.
    /// The value is the *key* in the default secret store, not the
    /// secret bytes themselves.
    #[secret]
    pub api_token: String,

    /// Feature-flag sub-table. Nested so `config push` writes the
    /// dotted key `feature.new_checkout` (translated to
    /// `feature__new_checkout` on Spin) — matching the existing
    /// handler that reads `feature.new_checkout` from the config
    /// store and the per-adapter seeds in `fastly.toml`/`spin.toml`.
    /// `#[validate(nested)]` makes the outer `validate()` call
    /// recurse into `FeatureConfig`'s rules.
    #[validate(nested)]
    pub feature: FeatureConfig,

    /// Free-form greeting surfaced by example handlers.
    pub greeting: String,

    /// Nested section — exercises the env-var overlay on a sub-table
    /// (`app_demo__SERVICE__TIMEOUT_MS=…`). `#[validate(nested)]`
    /// propagates the inner `range` rule on `timeout_ms` up to the
    /// outer `AppDemoConfig::validate()` — without it the inner
    /// validator silently no-ops.
    #[validate(nested)]
    pub service: ServiceConfig,

    /// Logical id of a secret store declared in `[stores.secrets].ids`
    /// in `edgezero.toml`. Resolved at runtime via
    /// `ctx.secret_store(&cfg.vault)?`. The app-demo manifest declares
    /// a single id (`"default"`), which is therefore the only valid
    /// value here — `config validate` enforces this.
    #[secret(store_ref)]
    pub vault: String,
}

#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(deny_unknown_fields)]
pub struct FeatureConfig {
    /// Toggles the (hypothetical) new-checkout code path. Exercises a
    /// non-string scalar through the env-var overlay
    /// (`app_demo__FEATURE__NEW_CHECKOUT=true`).
    pub new_checkout: bool,
}

#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    #[validate(range(min = 100_u32, max = 60_000_u32))]
    pub timeout_ms: u32,
}

#[cfg(test)]
#[expect(
    clippy::min_ident_chars,
    clippy::pattern_type_mismatch,
    reason = "`sort_by_key` and `Iterator::map` closure params use one-letter idents by convention; \
              `.sort_by_key(|entry| entry.0)` would shadow the inner tuple destructuring we use"
)]
mod tests {
    use super::*;
    use edgezero_core::app_config::{
        load_app_config, load_app_config_with_options, AppConfigLoadOptions, AppConfigMeta as _,
        SecretKind,
    };
    use std::env;
    use std::path::PathBuf;

    /// Resolve `examples/app-demo/app-demo.toml` from this test file's
    /// directory — `CARGO_MANIFEST_DIR` for `app-demo-core` is
    /// `examples/app-demo/crates/app-demo-core`, so the file lives two
    /// directories up.
    fn app_demo_toml_path() -> PathBuf {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest_dir
            .parent()
            .and_then(|crates_dir| crates_dir.parent())
            .expect("app-demo-core lives two dirs below the app-demo workspace root")
            .join("app-demo.toml")
    }

    #[test]
    fn loads_app_demo_toml_round_trip() {
        let path = app_demo_toml_path();
        // Disable the process-env overlay so a stray env var on the
        // developer's machine (or the sibling overlay test) can't
        // break this round-trip — we're asserting the on-disk values.
        // `AppConfigLoadOptions` is `#[non_exhaustive]`, so it must be
        // constructed via `Default::default()` and mutated.
        let mut opts = AppConfigLoadOptions::default();
        opts.env_overlay = false;
        let cfg = load_app_config_with_options::<AppDemoConfig>(&path, "app-demo", &opts)
            .expect("load AppDemoConfig from app-demo.toml");

        assert_eq!(cfg.greeting, "hello from app-demo");
        assert!(!cfg.feature.new_checkout);
        assert_eq!(cfg.api_token, "demo_api_token");
        assert_eq!(cfg.vault, "default");
        assert_eq!(cfg.service.timeout_ms, 1500);
    }

    #[test]
    fn secret_fields_metadata_matches_declarations() {
        let mut by_name: Vec<(&str, SecretKind)> = AppDemoConfig::SECRET_FIELDS
            .iter()
            .map(|f| (f.name, f.kind))
            .collect();
        by_name.sort_by_key(|(name, _)| *name);
        assert_eq!(
            by_name,
            vec![
                ("api_token", SecretKind::KeyInDefault),
                ("vault", SecretKind::StoreRef),
            ],
        );
    }

    #[test]
    fn nested_validator_rules_propagate_to_outer_validate() {
        // Regression: without `#[validate(nested)]` on the
        // `AppDemoConfig::service` field, the inner
        // `#[validate(range(min = 100, ...))]` on
        // `ServiceConfig::timeout_ms` silently no-ops. Write a
        // tempfile fixture with `timeout_ms = 50` so we don't race
        // the sibling env-overlay test over the shared process env
        // var, and load with the overlay disabled so the file
        // values are decisive.
        use std::io::Write as _;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().expect("tempfile");
        file.write_all(
            br#"
[config]
api_token = "x"
greeting = "hi"
vault = "default"

[config.feature]
new_checkout = false

[config.service]
timeout_ms = 50
"#,
        )
        .expect("write fixture");

        let mut opts = AppConfigLoadOptions::default();
        opts.env_overlay = false;
        let result = load_app_config_with_options::<AppDemoConfig>(file.path(), "app-demo", &opts);

        let err = result.expect_err("out-of-range nested field must error");
        let message = err.to_string();
        assert!(
            message.to_lowercase().contains("validation"),
            "error mentions validation: {message}"
        );
        assert!(
            message.contains("timeout_ms"),
            "error names the failing nested field: {message}"
        );
    }

    #[test]
    fn env_overlay_overrides_nested_value() {
        // Mutate process env in-place; the sibling round-trip test
        // uses `env_overlay: false`, so a parallel run can't be
        // affected by this var. The key is otherwise unique to this
        // test.
        const KEY: &str = "APP_DEMO__SERVICE__TIMEOUT_MS";
        env::set_var(KEY, "2500");

        let path = app_demo_toml_path();
        let cfg = load_app_config::<AppDemoConfig>(&path, "app-demo")
            .expect("load with env-overlay override");

        env::remove_var(KEY);

        assert_eq!(cfg.service.timeout_ms, 2500);
        // Unrelated keys keep their on-disk values.
        assert_eq!(cfg.greeting, "hello from app-demo");
    }
}
