//! Typed app-config loading.
//!
//! Loader for downstream `<name>.toml` files (e.g. `app-demo.toml`).
//! Reads the file's top-level table verbatim — there is no `[config]`
//! wrapper — optionally applies the `<APP_NAME>__<SECTION>__…<KEY>`
//! env-var overlay, and either:
//!
//! - Deserialises into a downstream `C: DeserializeOwned + Validate`
//!   and runs `validator::Validate::validate()` —
//!   [`load_app_config`] / [`load_app_config_with_options`].
//! - Returns the parsed root table as raw `toml::Value` for tools
//!   that don't have access to the typed struct (the raw `config
//!   push` flow) — [`load_app_config_raw`] /
//!   [`load_app_config_raw_with_options`].

use std::any;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use thiserror::Error;
use toml::de::Error as TomlDeError;
use toml::value::Datetime;
use toml::Value;
use validator::{Validate, ValidationErrors};

/// Per-field metadata emitted by `#[derive(AppConfig)]`. The
/// derive enumerates every field annotated with `#[secret]` /
/// `#[secret(store_ref)]`; `config validate` and `config push`
/// reflect over this array to gate secret-aware behaviour.
pub trait AppConfigMeta {
    /// Every `#[secret]` / `#[secret(store_ref)]` field on the struct.
    const SECRET_FIELDS: &'static [SecretField];
}

/// One field's worth of secret-annotation metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecretField {
    /// Whether the field's value is a key in the default secret store
    /// or the logical id of a `[stores.secrets]` entry.
    pub kind: SecretKind,
    /// Rust field name verbatim (no `serde(rename)` translation —
    /// `#[secret]` rejects renames at compile time).
    pub name: &'static str,
}

/// Discriminator on a [`SecretField`] capturing which secret-store
/// resolution the field participates in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretKind {
    /// `#[secret]` — the field's value is a key in the resolved
    /// default secret store.
    KeyInDefault,
    /// `#[secret(store_ref)]` — the field's value is the logical id
    /// of a `[stores.secrets]` declaration.
    StoreRef,
}

/// Options for the app-config loader.
///
/// Constructed with `Default::default()` (overlay on) by the simple
/// loader functions; `--no-env` on the CLI flips `env_overlay` to
/// `false`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct AppConfigLoadOptions {
    /// When `true`, apply the `<APP_NAME>__…__<KEY>` env-var overlay
    /// after parsing the file's root table; when `false`, the parsed
    /// values are used as-is.
    pub env_overlay: bool,
}

impl Default for AppConfigLoadOptions {
    #[inline]
    fn default() -> Self {
        Self { env_overlay: true }
    }
}

/// Errors returned by the app-config loader.
///
/// The TOML errors are boxed because `toml::de::Error` is large and a
/// fat `Err` variant would inflate every `Result<C, _>` on the loader's
/// hot path (`clippy::result_large_err`).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AppConfigError {
    /// Deserialising the file's top-level table into the typed `C`
    /// failed — missing required fields, wrong types, unknown fields
    /// (when the struct opts in to `#[serde(deny_unknown_fields)]`),
    /// etc.
    #[error("failed to deserialise {} into {target_type}: {source}", path.display())]
    Deserialize {
        path: PathBuf,
        target_type: &'static str,
        #[source]
        source: Box<TomlDeError>,
    },
    /// The env-overlay step failed — ambiguous sibling-key
    /// mapping, value not parseable against the existing TOML type,
    /// etc.
    #[error("env overlay failed for {}: {message}", path.display())]
    EnvOverlay { path: PathBuf, message: String },
    /// Failed to read the on-disk file (missing, permission denied,
    /// etc.).
    #[error("failed to read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The file exists but is not valid TOML.
    #[error("failed to parse {} as TOML: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<TomlDeError>,
    },
    /// `validator::Validate::validate()` rejected the parsed values
    /// (range / length / regex / custom validators).
    #[error("validation failed for {}: {source}", path.display())]
    Validation {
        path: PathBuf,
        #[source]
        source: Box<ValidationErrors>,
    },
}

/// Env-var lookup abstracted over the process env so tests can stub
/// it without manipulating `std::env`.
struct EnvLookup {
    vars: HashMap<String, String>,
}

impl EnvLookup {
    #[cfg(test)]
    fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            vars: pairs
                .into_iter()
                .map(|(key, val)| (key.into(), val.into()))
                .collect(),
        }
    }

    fn from_process_env() -> Self {
        Self {
            vars: env::vars().collect(),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }
}

/// Load and validate a typed app-config from `<name>.toml`.
///
/// `env_overlay` is on by default; pass [`AppConfigLoadOptions`]
/// explicitly via [`load_app_config_with_options`] to disable it.
///
/// `app_name` is `[app].name` (uppercased + `-`→`_`) used as the env-var
/// prefix when the overlay is on. It is accepted (not derived from the
/// file) so the loader is decoupled from manifest discovery — callers
/// (`config validate`, `config push`, the axum demo server) already have
/// it.
///
/// # Errors
/// See [`AppConfigError`].
#[inline]
pub fn load_app_config<C>(path: &Path, app_name: &str) -> Result<C, AppConfigError>
where
    C: DeserializeOwned + Validate + AppConfigMeta,
{
    load_app_config_with_options(path, app_name, &AppConfigLoadOptions::default())
}

/// [`load_app_config`] with an explicit [`AppConfigLoadOptions`].
///
/// # Errors
/// See [`AppConfigError`].
#[inline]
pub fn load_app_config_with_options<C>(
    path: &Path,
    app_name: &str,
    opts: &AppConfigLoadOptions,
) -> Result<C, AppConfigError>
where
    C: DeserializeOwned + Validate + AppConfigMeta,
{
    let config_table = load_app_config_raw_with_options(path, app_name, opts)?;
    let typed: C =
        config_table
            .try_into()
            .map_err(|source: TomlDeError| AppConfigError::Deserialize {
                path: path.to_path_buf(),
                target_type: any::type_name::<C>(),
                source: Box::new(source),
            })?;
    typed
        .validate()
        .map_err(|source| AppConfigError::Validation {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
    Ok(typed)
}

/// Read the file's root table as a raw `toml::Value`, with the env
/// overlay applied (when on). Used by `config push` and
/// other tools that don't have access to the typed struct.
///
/// # Errors
/// See [`AppConfigError`].
#[inline]
pub fn load_app_config_raw(path: &Path, app_name: &str) -> Result<Value, AppConfigError> {
    load_app_config_raw_with_options(path, app_name, &AppConfigLoadOptions::default())
}

/// [`load_app_config_raw`] with an explicit [`AppConfigLoadOptions`].
///
/// # Errors
/// See [`AppConfigError`].
#[inline]
pub fn load_app_config_raw_with_options(
    path: &Path,
    app_name: &str,
    opts: &AppConfigLoadOptions,
) -> Result<Value, AppConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| AppConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut document: Value = toml::from_str(&raw).map_err(|source| AppConfigError::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    if opts.env_overlay {
        apply_env_overlay(&mut document, app_name, path)?;
    }
    Ok(document)
}

/// Apply the `<APP_NAME>__<SECTION>__…__<KEY>` env-var overlay
/// against the parsed root table.
///
/// The overlay only overrides keys that already exist in the parsed
/// tree (the existing TOML value's type drives coercion of the env
/// string). Two sibling keys mapping to the same env segment is an
/// `AppConfigError::EnvOverlay`; a string that can't be coerced to
/// the existing type is also an `EnvOverlay` error.
fn apply_env_overlay(
    config_table: &mut Value,
    app_name: &str,
    path: &Path,
) -> Result<(), AppConfigError> {
    let prefix = app_name_prefix(app_name);
    let lookup = EnvLookup::from_process_env();
    walk_and_overlay(config_table, &prefix, &lookup, path)
}

/// Normalise an app name to the env-var prefix (`<APP_NAME>` form
/// from): uppercase, `-`→`_`. A single leading `_` from a
/// project name that starts with a digit is preserved.
fn app_name_prefix(app_name: &str) -> String {
    app_name.to_ascii_uppercase().replace('-', "_")
}

/// Parse `raw` (env string) into the same `toml::Value` variant as
/// `existing`. Parse failure → `AppConfigError::EnvOverlay`.
fn coerce_env_value(
    existing: &Value,
    raw: &str,
    env_var: &str,
    path: &Path,
) -> Result<Value, AppConfigError> {
    let coerced = match existing {
        Value::String(_) => Value::String(raw.to_owned()),
        Value::Integer(_) => raw
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|err| coercion_error(env_var, raw, "integer", &err.to_string(), path))?,
        Value::Float(_) => raw
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|err| coercion_error(env_var, raw, "float", &err.to_string(), path))?,
        Value::Boolean(_) => match raw {
            "true" | "1" => Value::Boolean(true),
            "false" | "0" => Value::Boolean(false),
            other => {
                return Err(coercion_error(
                    env_var,
                    other,
                    "boolean (true/false/1/0)",
                    "expected true/false/1/0",
                    path,
                ));
            }
        },
        Value::Datetime(_) => raw
            .parse::<Datetime>()
            .map(Value::Datetime)
            .map_err(|err| coercion_error(env_var, raw, "datetime", &err.to_string(), path))?,
        Value::Array(_) | Value::Table(_) => {
            return Err(AppConfigError::EnvOverlay {
                path: path.to_path_buf(),
                message: format!(
                    "env var `{env_var}` cannot override array / table values — \
                     env overlay supports scalar leaves only"
                ),
            });
        }
    };
    Ok(coerced)
}

fn coercion_error(
    env_var: &str,
    raw: &str,
    target: &str,
    detail: &str,
    path: &Path,
) -> AppConfigError {
    AppConfigError::EnvOverlay {
        path: path.to_path_buf(),
        message: format!("env var `{env_var}={raw}` cannot be coerced to {target}: {detail}"),
    }
}

/// Translate a config field name into its env-segment form:
/// uppercase, `_` left as-is. Sibling keys that produce the same
/// segment are rejected by the caller as ambiguous.
fn env_segment(field_name: &str) -> String {
    field_name.to_ascii_uppercase()
}

fn walk_and_overlay(
    node: &mut Value,
    env_prefix: &str,
    lookup: &EnvLookup,
    path: &Path,
) -> Result<(), AppConfigError> {
    let Value::Table(table) = node else {
        return Ok(());
    };

    // Detect ambiguous sibling-key mappings before applying any
    // overlay so a failure leaves the table untouched.
    let mut segment_owners: HashMap<String, String> = HashMap::new();
    for key in table.keys() {
        let segment = env_segment(key);
        if let Some(prior) = segment_owners.insert(segment.clone(), key.clone()) {
            return Err(AppConfigError::EnvOverlay {
                path: path.to_path_buf(),
                message: format!(
                    "sibling config keys `{prior}` and `{key}` both map to env segment \
                     `{segment}` under prefix `{env_prefix}__…`; rename one to disambiguate"
                ),
            });
        }
    }

    // Iterate over a snapshot of the keys so we can mutate `table`
    // inside the loop without borrowing it twice.
    let snapshot: Vec<String> = table.keys().cloned().collect();
    for key in snapshot {
        let segment = env_segment(&key);
        let next_prefix = format!("{env_prefix}__{segment}");
        let Some(value) = table.get_mut(&key) else {
            continue;
        };
        match value {
            Value::Table(_) => walk_and_overlay(value, &next_prefix, lookup, path)?,
            Value::String(_)
            | Value::Integer(_)
            | Value::Float(_)
            | Value::Boolean(_)
            | Value::Datetime(_)
            | Value::Array(_) => {
                if let Some(raw) = lookup.get(&next_prefix) {
                    *value = coerce_env_value(value, raw, &next_prefix, path)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::default_numeric_fallback,
    clippy::wildcard_enum_match_arm,
    reason = "test fixtures: `validator` range bounds default to the field's int type; \
              match arms in `expect_err` assertions intentionally collapse all unexpected \
              variants into a single panic"
)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::io::Write as _;
    use tempfile::NamedTempFile;

    // `AppConfigMeta` is hand-impl'd here rather than derived: the
    // `#[derive(AppConfig)]` proc macro emits absolute paths
    // (`::edgezero_core::…`) that don't resolve inside the defining
    // crate's own modules. The downstream integration test in
    // `edgezero-macros/tests/app_config_derive.rs` exercises the derive
    // itself; this fixture only needs the trait bound to satisfy
    // `load_app_config<C>`.
    #[derive(Debug, Deserialize, Validate, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct FixtureConfig {
        greeting: String,
        #[validate(range(min = 100, max = 60_000))]
        timeout_ms: u32,
    }

    impl AppConfigMeta for FixtureConfig {
        const SECRET_FIELDS: &'static [SecretField] = &[];
    }

    fn write_fixture(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("tempfile");
        file.write_all(contents.as_bytes()).expect("write");
        file
    }

    #[test]
    fn load_app_config_round_trips_a_valid_file() {
        let file = write_fixture(
            r#"
greeting = "hello"
timeout_ms = 1500
"#,
        );
        let cfg: FixtureConfig = load_app_config(file.path(), "fixture").expect("load");
        assert_eq!(
            cfg,
            FixtureConfig {
                greeting: "hello".to_owned(),
                timeout_ms: 1500,
            }
        );
    }

    #[test]
    fn load_app_config_errors_with_io_variant_for_missing_file() {
        let path = PathBuf::from("/definitely/not/a/real/path/app.toml");
        let err = load_app_config::<FixtureConfig>(&path, "fixture")
            .expect_err("missing file must error");
        assert!(
            matches!(err, AppConfigError::Io { .. }),
            "expected Io variant, got {err:?}"
        );
    }

    #[test]
    fn load_app_config_errors_with_parse_variant_for_bad_toml() {
        let file = write_fixture("{not toml");
        let err = load_app_config::<FixtureConfig>(file.path(), "fixture")
            .expect_err("bad TOML must error");
        assert!(
            matches!(err, AppConfigError::Parse { .. }),
            "expected Parse variant, got {err:?}"
        );
    }

    #[test]
    fn load_app_config_errors_with_deserialize_variant_for_unknown_fields() {
        let file = write_fixture(
            r#"
greeting = "hello"
timeout_ms = 1500
extra_unknown = "rejected by deny_unknown_fields"
"#,
        );
        let err = load_app_config::<FixtureConfig>(file.path(), "fixture")
            .expect_err("unknown field must error");
        assert!(
            matches!(err, AppConfigError::Deserialize { .. }),
            "expected Deserialize variant, got {err:?}"
        );
    }

    #[test]
    fn load_app_config_errors_with_validation_variant() {
        // `timeout_ms = 99` violates `range(min = 100, ..)`.
        let file = write_fixture(
            r#"
greeting = "hello"
timeout_ms = 99
"#,
        );
        let err = load_app_config::<FixtureConfig>(file.path(), "fixture")
            .expect_err("validation must error");
        assert!(
            matches!(err, AppConfigError::Validation { .. }),
            "expected Validation variant, got {err:?}"
        );
    }

    #[test]
    fn load_app_config_raw_returns_the_root_table() {
        let file = write_fixture(
            r#"
greeting = "hello"

[service]
timeout_ms = 1500
"#,
        );
        let raw = load_app_config_raw(file.path(), "fixture").expect("load raw");
        let table = raw.as_table().expect("raw value is a table");
        assert_eq!(table.get("greeting").and_then(Value::as_str), Some("hello"),);
        assert!(
            table.get("service").and_then(Value::as_table).is_some(),
            "nested [service] survives raw load"
        );
    }

    #[test]
    fn default_load_options_have_env_overlay_on() {
        assert_eq!(
            AppConfigLoadOptions::default(),
            AppConfigLoadOptions { env_overlay: true }
        );
    }

    // -- Env overlay ------------------------------------------------

    fn parse_root_table(contents: &str) -> Value {
        toml::from_str(contents).expect("parse fixture")
    }

    fn overlay_with_lookup(
        config_table: &mut Value,
        app_name: &str,
        pairs: &[(&str, &str)],
    ) -> Result<(), AppConfigError> {
        let lookup = EnvLookup::from_pairs(pairs.iter().copied());
        let prefix = app_name_prefix(app_name);
        walk_and_overlay(config_table, &prefix, &lookup, Path::new("fixture.toml"))
    }

    #[test]
    fn env_overlay_overrides_top_level_string() {
        let mut table = parse_root_table(
            r#"
greeting = "hello"
"#,
        );
        overlay_with_lookup(&mut table, "app-demo", &[("APP_DEMO__GREETING", "hola")])
            .expect("overlay");
        assert_eq!(table.get("greeting").and_then(Value::as_str), Some("hola"));
    }

    #[test]
    fn env_overlay_overrides_nested_integer_with_coercion() {
        let mut table = parse_root_table(
            "
[service]
timeout_ms = 1500
",
        );
        overlay_with_lookup(
            &mut table,
            "app-demo",
            &[("APP_DEMO__SERVICE__TIMEOUT_MS", "3000")],
        )
        .expect("overlay");
        assert_eq!(
            table
                .get("service")
                .and_then(Value::as_table)
                .and_then(|service| service.get("timeout_ms"))
                .and_then(Value::as_integer),
            Some(3000)
        );
    }

    #[test]
    fn env_overlay_coerces_boolean_from_true_false_or_numeric() {
        for (raw, expected) in [("true", true), ("false", false), ("1", true), ("0", false)] {
            let mut table = parse_root_table(
                "
feature_new_checkout = false
",
            );
            overlay_with_lookup(
                &mut table,
                "app-demo",
                &[("APP_DEMO__FEATURE_NEW_CHECKOUT", raw)],
            )
            .expect("overlay");
            assert_eq!(
                table.get("feature_new_checkout").and_then(Value::as_bool),
                Some(expected),
                "raw={raw:?}"
            );
        }
    }

    #[test]
    fn env_overlay_errors_when_value_cannot_be_coerced_to_existing_type() {
        let mut table = parse_root_table(
            "
[service]
timeout_ms = 1500
",
        );
        let err = overlay_with_lookup(
            &mut table,
            "app-demo",
            &[("APP_DEMO__SERVICE__TIMEOUT_MS", "not-a-number")],
        )
        .expect_err("non-numeric env value must error");
        match err {
            AppConfigError::EnvOverlay { message, .. } => {
                assert!(
                    message.contains("APP_DEMO__SERVICE__TIMEOUT_MS"),
                    "error names the env var: {message}"
                );
                assert!(
                    message.contains("integer"),
                    "error names the target type: {message}"
                );
            }
            other => panic!("expected EnvOverlay variant, got {other:?}"),
        }
    }

    #[test]
    fn env_overlay_rejects_sibling_keys_with_same_env_segment() {
        // `greeting_a` and `GREETING_A` would both translate to env
        // segment `GREETING_A` (uppercase). Since TOML keys are
        // case-sensitive but env segments aren't, we need a guard.
        let mut table = parse_root_table(
            r#"
greeting_a = "lower"
GREETING_A = "upper"
"#,
        );
        let err = overlay_with_lookup(&mut table, "app-demo", &[])
            .expect_err("ambiguous siblings must error");
        match err {
            AppConfigError::EnvOverlay { message, .. } => {
                assert!(
                    message.contains("GREETING_A"),
                    "names env segment: {message}"
                );
                assert!(
                    message.contains("rename one to disambiguate"),
                    "explains the remediation: {message}"
                );
            }
            other => panic!("expected EnvOverlay variant, got {other:?}"),
        }
    }

    #[test]
    fn env_overlay_disabled_skips_walker_entirely() {
        // With `env_overlay: false`, even when the env var is set the
        // parsed value is returned untouched. Uses a unique app-name
        // prefix so the temporary env var can't leak into other
        // tests run in parallel (cargo test does not isolate
        // process env between threads).
        let file = write_fixture(
            r#"
greeting = "hello"
timeout_ms = 1500
"#,
        );
        let app_name = "overlay_disabled_test";
        let env_key = "OVERLAY_DISABLED_TEST__GREETING";
        env::set_var(env_key, "should-be-ignored");
        let cfg = load_app_config_with_options::<FixtureConfig>(
            file.path(),
            app_name,
            &AppConfigLoadOptions { env_overlay: false },
        )
        .expect("load");
        env::remove_var(env_key);
        assert_eq!(cfg.greeting, "hello", "overlay disabled: file value wins");
    }

    #[test]
    fn env_overlay_only_overrides_existing_keys() {
        // An env var for a key that is not already present in the
        // parsed table is silently ignored (the overlay never adds
        // new keys — "env vars override existing keys only").
        let mut table = parse_root_table(
            r#"
greeting = "hello"
"#,
        );
        overlay_with_lookup(
            &mut table,
            "app-demo",
            &[("APP_DEMO__UNKNOWN_KEY", "ignored")],
        )
        .expect("overlay");
        assert!(
            table.get("unknown_key").is_none(),
            "overlay must not synthesise keys"
        );
        assert_eq!(
            table.get("greeting").and_then(Value::as_str),
            Some("hello"),
            "existing key untouched when no env var present"
        );
    }

    #[test]
    fn app_name_prefix_uppercases_and_translates_dash_to_underscore() {
        assert_eq!(app_name_prefix("app-demo"), "APP_DEMO");
        assert_eq!(app_name_prefix("my_app"), "MY_APP");
        assert_eq!(app_name_prefix("a-b-c"), "A_B_C");
    }
}
