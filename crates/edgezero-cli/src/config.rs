//! `config validate` (spec §10).
//!
//! Two entry points share the same checks against the manifest, the
//! app-config file, and (when `spin` is in the adapter set) the Spin
//! key-syntax / component-discovery rules:
//!
//! - [`run_config_validate`] — raw flow. Loads the file's root
//!   table as a [`toml::Value`] only; the typed deserialise /
//!   `validator` / secret checks are skipped because no `C` is in
//!   scope. The default `edgezero` binary uses this.
//! - [`run_config_validate_typed`] — typed flow. Adds typed
//!   deserialisation, `validator::Validate::validate()`, the
//!   `#[secret]` / `#[secret(store_ref)]` checks, and the Spin
//!   config/secret collision check (§6.7 check 2). Downstream
//!   project CLIs that own an app-config struct wire this up.
//!
//! Both run the manifest through [`ManifestLoader`] (which itself
//! validates everything per §3) and reject the typed app-config's
//! env-overlay unless `--no-env` is passed, so the validation sees
//! the values the runtime would.

use crate::args::ConfigValidateArgs;
use edgezero_core::app_config::{
    self, AppConfigError, AppConfigLoadOptions, AppConfigMeta, SecretField, SecretKind,
};
use edgezero_core::manifest::{Manifest, ManifestLoader};
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use toml::value::Table;
use toml::Value;
use validator::Validate;

/// Pre-loaded state shared by the raw and typed flows.
struct ValidationContext {
    /// Resolved app-config TOML path. Either the explicit
    /// `--app-config`, or `<app_name>.toml` next to the manifest.
    app_config_path: PathBuf,
    /// `[app].name` from the manifest. Drives the env-overlay
    /// prefix and (when `--app-config` is unset) the default app-
    /// config filename.
    app_name: String,
    args_strict: bool,
    /// Validated manifest, kept alive for the duration of the
    /// validation run. Borrowed everywhere via [`Self::manifest`].
    manifest_loader: ManifestLoader,
    /// Path the manifest was loaded from — kept so error messages
    /// can name the user-visible file.
    manifest_path: PathBuf,
    /// Raw root table of `<name>.toml` — loaded with the same
    /// overlay setting the typed flow will use, so the raw Spin
    /// key-syntax check sees the same values.
    raw_config: Value,
}

impl ValidationContext {
    fn manifest(&self) -> &Manifest {
        self.manifest_loader.manifest()
    }
}

/// Raw flow — no typed `C`. Runs every check the typed flow runs
/// *except* the typed deserialise, the validator rules, the secret
/// presence / store-ref checks, and the Spin config-vs-secret
/// collision (§6.7 check 2), which all require `AppConfigMeta`.
///
/// # Errors
/// Returns a human-readable error string on any validation failure.
#[inline]
pub fn run_config_validate(args: &ConfigValidateArgs) -> Result<(), String> {
    let ctx = load_validation_context(args)?;
    run_shared_checks(&ctx)?;
    Ok(())
}

/// Typed flow — adds the checks that need the user's `C` struct.
///
/// # Errors
/// Returns a human-readable error string on any validation failure.
#[inline]
pub fn run_config_validate_typed<C>(args: &ConfigValidateArgs) -> Result<(), String>
where
    C: DeserializeOwned + Validate + AppConfigMeta,
{
    let ctx = load_validation_context(args)?;
    run_shared_checks(&ctx)?;

    // Typed deserialise + validator pass. `load_app_config_with_options`
    // applies the env overlay on its own, so we hand it the raw on-disk
    // path again rather than threading `ctx.raw_config` through.
    let mut opts = AppConfigLoadOptions::default();
    opts.env_overlay = !args.no_env;
    let typed: C =
        app_config::load_app_config_with_options::<C>(&ctx.app_config_path, &ctx.app_name, &opts)
            .map_err(|err| format_app_config_error(&err))?;

    typed_secret_checks(&typed, &ctx)?;
    // Spin's flat variable namespace can collide with a secret value
    // resolved via `#[secret]` — see §6.7 check 2. The collision check
    // self-gates: it returns `Ok` when Spin isn't in the adapter set,
    // so the context stays adapter-agnostic.
    spin_config_secret_collision(&ctx, C::SECRET_FIELDS)?;

    Ok(())
}

fn load_validation_context(args: &ConfigValidateArgs) -> Result<ValidationContext, String> {
    let manifest_loader = ManifestLoader::from_path(&args.manifest)
        .map_err(|err| format!("failed to load {}: {err}", args.manifest.display()))?;

    // Spec §3: every project carries a `[app].name`. Without it we
    // can't compute the env-overlay prefix or resolve the default
    // app-config path.
    let app_name = manifest_loader.manifest().app.name.clone().ok_or_else(|| {
        format!(
            "{} has no `[app].name` — required to resolve the typed app-config",
            args.manifest.display()
        )
    })?;

    let app_config_path = resolve_app_config_path(args, &args.manifest, &app_name);

    // Load the raw root table once. The typed flow will re-load it
    // via `load_app_config_with_options::<C>` to drive deserialise +
    // validator; we keep this copy for shared checks (Spin key
    // syntax, component discovery) that don't need `C`.
    let mut opts = AppConfigLoadOptions::default();
    opts.env_overlay = !args.no_env;
    let raw_config =
        app_config::load_app_config_raw_with_options(&app_config_path, &app_name, &opts)
            .map_err(|err| format_app_config_error(&err))?;

    Ok(ValidationContext {
        app_config_path,
        app_name,
        args_strict: args.strict,
        manifest_loader,
        manifest_path: args.manifest.clone(),
        raw_config,
    })
}

fn resolve_app_config_path(
    args: &ConfigValidateArgs,
    manifest_path: &Path,
    app_name: &str,
) -> PathBuf {
    if let Some(explicit) = &args.app_config {
        return explicit.clone();
    }
    let manifest_dir = manifest_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let default_name = format!("{app_name}.toml");
    manifest_dir.map_or_else(
        || PathBuf::from(&default_name),
        |dir| dir.join(&default_name),
    )
}

fn run_shared_checks(ctx: &ValidationContext) -> Result<(), String> {
    // Each Spin check self-gates on `manifest.adapters.contains_key("spin")`,
    // so the context stays adapter-agnostic and the call site reads as a
    // flat list of contract checks.
    spin_key_syntax_check(ctx.manifest(), &ctx.raw_config)?;
    spin_component_discovery(ctx.manifest(), &ctx.manifest_path)?;
    if ctx.args_strict {
        strict_capability_completeness(ctx.manifest())?;
        strict_handler_paths(ctx.manifest())?;
    }
    Ok(())
}

// -------------------------------------------------------------------
// Typed secret checks (§6.8)
// -------------------------------------------------------------------

fn typed_secret_checks<C: AppConfigMeta>(
    _typed: &C,
    ctx: &ValidationContext,
) -> Result<(), String> {
    let raw_table = ctx
        .raw_config
        .as_table()
        .ok_or_else(|| "raw app-config was not a TOML table after load".to_owned())?;

    for field in C::SECRET_FIELDS {
        let value = raw_table
            .get(field.name)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "{}: `#[secret]` field `{}` is missing or not a string at the top level",
                    ctx.app_config_path.display(),
                    field.name
                )
            })?;
        if value.is_empty() {
            return Err(format!(
                "{}: `#[secret]` field `{}` must be non-empty",
                ctx.app_config_path.display(),
                field.name
            ));
        }
        match field.kind {
            SecretKind::KeyInDefault => {
                if ctx.manifest().stores.secrets.is_none() {
                    return Err(format!(
                        "{}: `#[secret]` field `{}` requires `[stores.secrets]` to be declared in {}",
                        ctx.app_config_path.display(),
                        field.name,
                        ctx.manifest_path.display()
                    ));
                }
            }
            SecretKind::StoreRef => {
                let secrets = ctx.manifest().stores.secrets.as_ref().ok_or_else(|| {
                    format!(
                        "{}: `#[secret(store_ref)]` field `{}` requires `[stores.secrets]` to be declared in {}",
                        ctx.app_config_path.display(),
                        field.name,
                        ctx.manifest_path.display()
                    )
                })?;
                if !secrets.ids.iter().any(|id| id == value) {
                    return Err(format!(
                        "{}: `#[secret(store_ref)]` field `{}` = {:?} is not in [stores.secrets].ids ({:?})",
                        ctx.app_config_path.display(),
                        field.name,
                        value,
                        secrets.ids
                    ));
                }
            }
        }
    }
    Ok(())
}

// -------------------------------------------------------------------
// Spin checks (spec §6.7)
// -------------------------------------------------------------------

fn spin_key_syntax_check(manifest: &Manifest, raw_config: &Value) -> Result<(), String> {
    if !manifest.adapters.contains_key("spin") {
        return Ok(());
    }
    let table = raw_config
        .as_table()
        .ok_or_else(|| "raw app-config was not a TOML table after load".to_owned())?;
    for key in flatten_keys(table) {
        let spin_var = key.replace('.', "__");
        if !is_valid_spin_key(&spin_var) {
            return Err(format!(
                "config key `{key}` translates to Spin variable `{spin_var}`, which does not match `^[a-z][a-z0-9_]*$`"
            ));
        }
    }
    Ok(())
}

fn is_valid_spin_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn spin_config_secret_collision(
    ctx: &ValidationContext,
    secret_fields: &[SecretField],
) -> Result<(), String> {
    if !ctx.manifest().adapters.contains_key("spin") {
        return Ok(());
    }
    let raw_table = ctx
        .raw_config
        .as_table()
        .ok_or_else(|| "raw app-config was not a TOML table after load".to_owned())?;

    let mut seen: HashSet<String> = HashSet::new();
    for key in flatten_keys(raw_table) {
        let spin_var = key.replace('.', "__");
        if !seen.insert(spin_var.clone()) {
            return Err(format!(
                "duplicate Spin variable `{spin_var}` derived from config key `{key}`"
            ));
        }
    }
    for field in secret_fields {
        // Spec §6.7 check 2: the collision set is {flattened config
        // keys} ∪ {plain `#[secret]` values}. `#[secret(store_ref)]`
        // values are *logical store ids* resolved at runtime — they
        // never enter Spin's flat variable namespace and so cannot
        // collide. Skip them.
        if !matches!(field.kind, SecretKind::KeyInDefault) {
            continue;
        }
        let Some(value) = raw_table.get(field.name).and_then(Value::as_str) else {
            continue; // typed_secret_checks would have surfaced the absence already
        };
        let spin_var = value.replace('.', "__");
        if !seen.insert(spin_var.clone()) {
            return Err(format!(
                "Spin variable `{spin_var}` (from `#[secret]` field `{}`) collides with a config key under the same name; Spin's flat variable namespace cannot disambiguate them",
                field.name
            ));
        }
    }
    Ok(())
}

fn flatten_keys(table: &Table) -> Vec<String> {
    let mut out = Vec::new();
    flatten_keys_into(table, "", &mut out);
    out
}

fn flatten_keys_into(table: &Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table {
        let full = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if let Some(nested) = value.as_table() {
            flatten_keys_into(nested, &full, out);
        } else {
            out.push(full);
        }
    }
}

fn spin_component_discovery(manifest: &Manifest, manifest_path: &Path) -> Result<(), String> {
    // Caller guarantees `has_spin_adapter()`; the `else` branch covers
    // the (impossible) case so we don't lean on `.expect()`.
    let Some(spin) = manifest.adapters.get("spin") else {
        return Ok(());
    };
    let Some(rel_spin_toml) = &spin.adapter.manifest else {
        return Err(format!(
            "{}: [adapters.spin.adapter].manifest must point at spin.toml for Spin component discovery",
            manifest_path.display()
        ));
    };
    let manifest_dir = manifest_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let spin_path = manifest_dir.map_or_else(
        || PathBuf::from(rel_spin_toml),
        |dir| dir.join(rel_spin_toml),
    );

    let raw = fs::read_to_string(&spin_path).map_err(|err| {
        format!(
            "failed to read spin manifest at {}: {err}",
            spin_path.display()
        )
    })?;
    let parsed: Value = toml::from_str(&raw)
        .map_err(|err| format!("failed to parse {} as TOML: {err}", spin_path.display()))?;
    let component_ids = collect_spin_component_ids(&parsed);

    if component_ids.is_empty() {
        return Err(format!(
            "{}: no [component.*] declarations found",
            spin_path.display()
        ));
    }

    // An explicit selector must always name a declared component, even
    // when there is exactly one — a typo would otherwise silently pass
    // here and only blow up later in `config push` / `provision`.
    if let Some(selector) = &spin.adapter.component {
        if component_ids.iter().any(|id| id == selector) {
            return Ok(());
        }
        return Err(format!(
            "[adapters.spin.adapter].component = {:?} is not declared in {} (available: {})",
            selector,
            spin_path.display(),
            component_ids.join(", ")
        ));
    }

    // No selector — auto-select only when there is exactly one
    // component; otherwise force the user to pick.
    if component_ids.len() == 1 {
        return Ok(());
    }
    Err(format!(
        "{} declares {} components ({}) but [adapters.spin.adapter].component is unset; set one explicitly",
        spin_path.display(),
        component_ids.len(),
        component_ids.join(", ")
    ))
}

fn collect_spin_component_ids(parsed: &Value) -> Vec<String> {
    parsed
        .as_table()
        .and_then(|root| root.get("component"))
        .and_then(Value::as_table)
        .map(|components| components.keys().cloned().collect())
        .unwrap_or_default()
}

// -------------------------------------------------------------------
// --strict checks (spec §10)
// -------------------------------------------------------------------

fn strict_capability_completeness(manifest: &Manifest) -> Result<(), String> {
    // Spec §6.6 capability matrix. Hard-coded here rather than threaded
    // through the adapter registry because the registry is feature-gated
    // per platform and the validator must run regardless of build
    // features.
    for (kind, maybe_decl) in [
        ("kv", manifest.stores.kv.as_ref()),
        ("config", manifest.stores.config.as_ref()),
        ("secrets", manifest.stores.secrets.as_ref()),
    ] {
        let Some(declaration) = maybe_decl else {
            continue;
        };
        if declaration.ids.len() <= 1 {
            continue;
        }
        for adapter in manifest.adapters.keys() {
            if is_single_store_adapter(adapter, kind) {
                return Err(format!(
                    "adapter `{adapter}` is Single-capable for {kind} stores (spec §6.6) but [stores.{kind}].ids declares {} ids; pick one or drop the adapter",
                    declaration.ids.len()
                ));
            }
        }
    }
    Ok(())
}

fn is_single_store_adapter(adapter: &str, kind: &str) -> bool {
    // Spec §6.6 capability matrix:
    // - axum/cloudflare are Single only for `secrets` (env vars /
    //   worker secrets); both are Multi for KV and Config.
    // - fastly is Multi across the board.
    // - spin is Multi for KV (label-backed) but Single for Config and
    //   Secrets (flat-variable namespace).
    matches!(
        (adapter, kind),
        ("axum" | "cloudflare", "secrets") | ("spin", "config" | "secrets")
    )
}

fn strict_handler_paths(manifest: &Manifest) -> Result<(), String> {
    for trigger in &manifest.triggers.http {
        let Some(handler) = &trigger.handler else {
            continue;
        };
        if !is_valid_handler_path(handler) {
            return Err(format!(
                "trigger {} handler `{handler}` is not a well-formed Rust path (expected `crate::module::function`)",
                trigger.id.as_deref().unwrap_or(&trigger.path)
            ));
        }
    }
    Ok(())
}

fn is_valid_handler_path(handler: &str) -> bool {
    let segments: Vec<&str> = handler.split("::").collect();
    if segments.len() < 2 {
        return false;
    }
    segments.iter().all(|segment| is_rust_ident(segment))
}

fn is_rust_ident(segment: &str) -> bool {
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

// -------------------------------------------------------------------
// Error formatting
// -------------------------------------------------------------------

fn format_app_config_error(err: &AppConfigError) -> String {
    err.to_string()
}

#[cfg(test)]
#[expect(
    clippy::default_numeric_fallback,
    reason = "the validator `range(min = 100, max = 60_000)` bounds default to the field's int type"
)]
mod tests {
    use super::*;
    use edgezero_core::app_config::SecretField;
    use serde::Deserialize;
    use tempfile::TempDir;

    // ---------- shared fixtures ----------

    const FIXTURE_APP_CONFIG: &str = r#"
api_token = "demo_api_token"
greeting = "hello"
vault = "default"

[service]
timeout_ms = 1500
"#;

    const VALID_APP_CONFIG: &str = r#"
api_token = "demo_api_token"
greeting = "hello"
"#;

    const VALID_MANIFEST: &str = r#"
[app]
name = "demo-app"
entry = "crates/demo-app-core"

[adapters.axum.adapter]
crate = "crates/demo-app-adapter-axum"

[adapters.axum.commands]
build = "cargo build"
deploy = "echo deploy"
serve = "cargo run"

[stores.secrets]
ids = ["default"]
"#;

    const VALID_SPIN_TOML: &str = r#"
spin_manifest_version = 2

[application]
name = "demo-app"
version = "0.1.0"

[[trigger.http]]
route = "/..."
component = "demo"

[component.demo]
source = "target/wasm32-wasip1/release/demo.wasm"
"#;

    /// `AppDemoConfig`-shaped fixture: `greeting` + `api_token` (a
    /// `#[secret]`) + `vault` (a `#[secret(store_ref)]`) + nested
    /// `service`.
    #[derive(Debug, Deserialize, Validate)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "fields are read by serde's deserialize and validator's validate; Rust's dead-code analysis can't see those paths"
    )]
    struct FixtureConfig {
        api_token: String,
        #[validate(length(min = 1_u64))]
        greeting: String,
        #[validate(nested)]
        service: FixtureServiceConfig,
        vault: String,
    }

    #[derive(Debug, Deserialize, Validate)]
    #[serde(deny_unknown_fields)]
    struct FixtureServiceConfig {
        #[validate(range(min = 100, max = 60_000))]
        timeout_ms: u32,
    }

    impl AppConfigMeta for FixtureConfig {
        const SECRET_FIELDS: &'static [SecretField] = &[
            SecretField {
                kind: SecretKind::KeyInDefault,
                name: "api_token",
            },
            SecretField {
                kind: SecretKind::StoreRef,
                name: "vault",
            },
        ];
    }

    fn setup_project(manifest: &str, app_config: &str) -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new().expect("temp dir");
        let manifest_path = dir.path().join("edgezero.toml");
        let app_config_path = dir.path().join("demo-app.toml");
        fs::write(&manifest_path, manifest).expect("write manifest");
        fs::write(&app_config_path, app_config).expect("write app config");
        (dir, manifest_path, app_config_path)
    }

    fn args_for(manifest: &Path) -> ConfigValidateArgs {
        ConfigValidateArgs {
            app_config: None,
            manifest: manifest.to_path_buf(),
            no_env: true, // tests don't want env leakage
            strict: false,
        }
    }

    // ---------- raw flow ----------

    #[test]
    fn raw_validates_a_well_formed_project() {
        let (_dir, manifest, _) = setup_project(VALID_MANIFEST, VALID_APP_CONFIG);
        run_config_validate(&args_for(&manifest)).expect("valid project passes");
    }

    #[test]
    fn raw_errors_on_unknown_manifest_path() {
        let dir = TempDir::new().expect("temp dir");
        let bogus = dir.path().join("nope.toml");
        let err = run_config_validate(&args_for(&bogus)).expect_err("missing manifest must error");
        assert!(err.contains("nope.toml"), "error names the path: {err}");
    }

    #[test]
    fn raw_errors_on_bad_app_config_toml() {
        let (_dir, manifest, app_config) = setup_project(VALID_MANIFEST, "{not toml");
        let err = run_config_validate(&args_for(&manifest)).expect_err("bad toml must error");
        assert!(
            err.contains(&app_config.display().to_string()),
            "error names the bad file: {err}"
        );
    }

    #[test]
    fn raw_errors_when_manifest_app_name_missing() {
        let manifest = r#"
[adapters.axum.adapter]
crate = "crates/demo"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#;
        let (_dir, manifest_path, _) = setup_project(manifest, VALID_APP_CONFIG);
        let err = run_config_validate(&args_for(&manifest_path))
            .expect_err("missing [app].name must error");
        assert!(
            err.contains("`[app].name`"),
            "error names the missing field: {err}"
        );
    }

    // ---------- typed flow ----------

    #[test]
    fn typed_validates_a_well_formed_project() {
        let (_dir, manifest, _) = setup_project(VALID_MANIFEST, FIXTURE_APP_CONFIG);
        run_config_validate_typed::<FixtureConfig>(&args_for(&manifest))
            .expect("valid typed project passes");
    }

    #[test]
    fn typed_errors_on_unknown_field() {
        let app_config = r#"
api_token = "x"
greeting = "hi"
vault = "default"
extra_unknown = "rejected"

[service]
timeout_ms = 1500
"#;
        let (_dir, manifest, _) = setup_project(VALID_MANIFEST, app_config);
        let err = run_config_validate_typed::<FixtureConfig>(&args_for(&manifest))
            .expect_err("unknown field must error");
        assert!(
            err.contains("extra_unknown") || err.to_lowercase().contains("unknown"),
            "error mentions the unknown field: {err}"
        );
    }

    #[test]
    fn typed_errors_on_validator_rule_failure() {
        let app_config = r#"
api_token = "x"
greeting = "hi"
vault = "default"

[service]
timeout_ms = 50
"#;
        let (_dir, manifest, _) = setup_project(VALID_MANIFEST, app_config);
        let err = run_config_validate_typed::<FixtureConfig>(&args_for(&manifest))
            .expect_err("validator rule must error");
        assert!(
            err.to_lowercase().contains("validation"),
            "error mentions validation: {err}"
        );
    }

    #[test]
    fn typed_errors_on_empty_secret_field() {
        let app_config = r#"
api_token = ""
greeting = "hi"
vault = "default"

[service]
timeout_ms = 1500
"#;
        let (_dir, manifest, _) = setup_project(VALID_MANIFEST, app_config);
        let err = run_config_validate_typed::<FixtureConfig>(&args_for(&manifest))
            .expect_err("empty secret must error");
        assert!(
            err.contains("api_token") && err.contains("non-empty"),
            "error names the empty secret field: {err}"
        );
    }

    #[test]
    fn typed_errors_when_store_ref_value_not_in_ids() {
        let app_config = r#"
api_token = "x"
greeting = "hi"
vault = "missing-id"

[service]
timeout_ms = 1500
"#;
        let (_dir, manifest, _) = setup_project(VALID_MANIFEST, app_config);
        let err = run_config_validate_typed::<FixtureConfig>(&args_for(&manifest))
            .expect_err("store_ref miss must error");
        assert!(
            err.contains("vault") && err.contains("missing-id"),
            "error names both the field and the bad value: {err}"
        );
    }

    #[test]
    fn typed_errors_when_secret_in_default_lacks_stores_secrets() {
        let manifest_without_secrets = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#;
        let (_dir, manifest, _) = setup_project(manifest_without_secrets, FIXTURE_APP_CONFIG);
        let err = run_config_validate_typed::<FixtureConfig>(&args_for(&manifest))
            .expect_err("missing [stores.secrets] must error");
        assert!(
            err.contains("[stores.secrets]"),
            "error names the missing manifest section: {err}"
        );
    }

    // ---------- Spin checks (spec §6.7) ----------

    fn spin_manifest(extra_section: &str) -> String {
        format!(
            r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.secrets]
ids = ["default"]
{extra_section}
"#
        )
    }

    fn write_spin_toml(dir: &Path, contents: &str) {
        fs::write(dir.join("spin.toml"), contents).expect("write spin.toml");
    }

    #[test]
    fn spin_key_syntax_rejects_uppercase_top_level_key() {
        let app_config = r#"
api_token = "x"
GREETING = "hi"
"#;
        let (dir, manifest, _) = setup_project(&spin_manifest(""), app_config);
        write_spin_toml(dir.path(), VALID_SPIN_TOML);
        let err = run_config_validate(&args_for(&manifest)).expect_err("uppercase key must error");
        assert!(
            err.contains("GREETING") && err.contains("Spin"),
            "error names the bad key + Spin: {err}"
        );
    }

    #[test]
    fn spin_key_syntax_rejects_dash_in_key() {
        let app_config = r#"
api-token = "x"
"#;
        let (dir, manifest, _) = setup_project(&spin_manifest(""), app_config);
        write_spin_toml(dir.path(), VALID_SPIN_TOML);
        let err = run_config_validate(&args_for(&manifest)).expect_err("dashed key must error");
        assert!(err.contains("api-token"), "error names the bad key: {err}");
    }

    #[test]
    fn spin_component_discovery_errors_on_zero_components() {
        let spin_toml = r#"
spin_manifest_version = 2

[application]
name = "demo-app"
version = "0.1.0"
"#;
        let (dir, manifest, _) = setup_project(&spin_manifest(""), VALID_APP_CONFIG);
        write_spin_toml(dir.path(), spin_toml);
        let err =
            run_config_validate(&args_for(&manifest)).expect_err("no [component.*] must error");
        assert!(
            err.contains("no [component.*]"),
            "error explains the absence: {err}"
        );
    }

    #[test]
    fn spin_component_discovery_errors_when_multi_unselected() {
        let spin_toml = r#"
spin_manifest_version = 2
[application]
name = "demo-app"
version = "0.1.0"
[component.alpha]
source = "a.wasm"
[component.beta]
source = "b.wasm"
"#;
        let (dir, manifest, _) = setup_project(&spin_manifest(""), VALID_APP_CONFIG);
        write_spin_toml(dir.path(), spin_toml);
        let err = run_config_validate(&args_for(&manifest))
            .expect_err("multi-component without selector must error");
        assert!(
            err.contains("alpha") && err.contains("beta") && err.contains("component"),
            "error lists the candidates: {err}"
        );
    }

    #[test]
    fn spin_component_discovery_rejects_bad_selector_against_single_component() {
        // Regression: a typo in `[adapters.spin.adapter].component`
        // used to pass when `spin.toml` declared exactly one
        // component because the auto-select path returned early
        // before checking the selector. A wrong id must fail here so
        // it doesn't blow up later in `config push` / `provision`.
        let spin_toml = r#"
spin_manifest_version = 2
[application]
name = "demo-app"
version = "0.1.0"
[component.actual]
source = "a.wasm"
"#;
        let manifest_str = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"
component = "typo"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.secrets]
ids = ["default"]
"#;
        let (dir, manifest, _) = setup_project(manifest_str, VALID_APP_CONFIG);
        write_spin_toml(dir.path(), spin_toml);
        let err = run_config_validate(&args_for(&manifest))
            .expect_err("typo'd selector against single component must error");
        assert!(
            err.contains("typo") && err.contains("actual"),
            "error names both the bad selector and the available id: {err}"
        );
    }

    #[test]
    fn spin_component_discovery_accepts_explicit_selector() {
        let spin_toml = r#"
spin_manifest_version = 2
[application]
name = "demo-app"
version = "0.1.0"
[component.alpha]
source = "a.wasm"
[component.beta]
source = "b.wasm"
"#;
        let manifest_str = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"
component = "alpha"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.secrets]
ids = ["default"]
"#;
        let (dir, manifest, _) = setup_project(manifest_str, VALID_APP_CONFIG);
        write_spin_toml(dir.path(), spin_toml);
        run_config_validate(&args_for(&manifest))
            .expect("explicit selector matching a declared component passes");
    }

    #[test]
    fn spin_config_secret_collision_ignores_store_ref_values() {
        // Regression: `#[secret(store_ref)]` values are logical
        // store ids (resolved at runtime), not Spin variable names —
        // they must not enter the Spin collision set. Earlier the
        // walker treated every SECRET_FIELDS entry as a potential
        // Spin var, so a perfectly valid `vault = "default"` plus a
        // config key whose flattened name happened to be `default`
        // would falsely trip a collision.
        //
        // We exercise the path the typed flow takes when a
        // `store_ref` value coincides with a real config key. The
        // raw flow already tolerated it; the typed flow used to
        // reject and should now pass.

        // Reuse the FixtureConfig shape but allow the extra `default`
        // key via a dedicated struct — the regression is about the
        // Spin walker, not about deserialisation. Items must precede
        // the test-local `let` bindings (clippy::items_after_statements).
        #[derive(Debug, Deserialize, Validate)]
        #[expect(dead_code, reason = "fields are read by serde/validator only")]
        struct StoreRefRegressionConfig {
            api_token: String,
            default: String,
            #[validate(length(min = 1_u64))]
            greeting: String,
            #[validate(nested)]
            service: FixtureServiceConfig,
            vault: String,
        }
        impl AppConfigMeta for StoreRefRegressionConfig {
            const SECRET_FIELDS: &'static [SecretField] = &[
                SecretField {
                    kind: SecretKind::KeyInDefault,
                    name: "api_token",
                },
                SecretField {
                    kind: SecretKind::StoreRef,
                    name: "vault",
                },
            ];
        }

        let manifest = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.secrets]
ids = ["default"]
"#;
        let app_config = r#"
api_token = "demo_api_token"
default = "shared-name"
greeting = "hi"
vault = "default"

[service]
timeout_ms = 1500
"#;
        let (dir, manifest_path, _) = setup_project(manifest, app_config);
        write_spin_toml(dir.path(), VALID_SPIN_TOML);
        run_config_validate_typed::<StoreRefRegressionConfig>(&args_for(&manifest_path))
            .expect("store_ref value coinciding with a config key must not collide");
    }

    #[test]
    fn spin_config_secret_collision_typed_only() {
        // `api_token = "greeting"` makes both the config key
        // `greeting` and the secret-store key derived from
        // `api_token`'s value translate to the same Spin variable
        // `greeting`. Typed flow must reject; raw flow can't see it.
        let app_config = r#"
api_token = "greeting"
greeting = "hi"
vault = "default"

[service]
timeout_ms = 1500
"#;
        let (dir, manifest, _) = setup_project(&spin_manifest(""), app_config);
        write_spin_toml(dir.path(), VALID_SPIN_TOML);

        // Raw flow tolerates it (no SECRET_FIELDS).
        run_config_validate(&args_for(&manifest)).expect("raw flow can't detect the collision");

        // Typed flow detects it.
        let err = run_config_validate_typed::<FixtureConfig>(&args_for(&manifest))
            .expect_err("typed flow must detect the collision");
        assert!(
            err.contains("greeting") && err.contains("collides"),
            "error names the colliding name: {err}"
        );
    }

    // ---------- --strict checks ----------

    #[test]
    fn strict_capability_completeness_rejects_single_adapter_with_multi_ids() {
        // Spin's secrets capability is Single — declaring two ids
        // breaks the contract under --strict.
        let manifest = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.secrets]
ids = ["alpha", "beta"]
default = "alpha"
"#;
        let (dir, manifest_path, _) = setup_project(manifest, VALID_APP_CONFIG);
        write_spin_toml(dir.path(), VALID_SPIN_TOML);
        let mut args = args_for(&manifest_path);
        args.strict = true;
        let err = run_config_validate(&args)
            .expect_err("Single-capable adapter with multi-id store must error");
        assert!(
            err.contains("spin") && err.contains("Single") && err.contains("secrets"),
            "error names adapter + capability: {err}"
        );
    }

    #[test]
    fn strict_handler_paths_rejects_malformed_handler() {
        let manifest = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[[triggers.http]]
id = "root"
path = "/"
methods = ["GET"]
handler = "not_a_path"

[stores.secrets]
ids = ["default"]
"#;
        let (_dir, manifest_path, _) = setup_project(manifest, VALID_APP_CONFIG);
        let mut args = args_for(&manifest_path);
        args.strict = true;
        let err =
            run_config_validate(&args).expect_err("malformed handler must error under --strict");
        assert!(
            err.contains("not_a_path") && err.contains("Rust path"),
            "error names the bad handler: {err}"
        );
    }

    // ---------- helpers ----------

    #[test]
    fn is_valid_spin_key_accepts_lowercase_with_digits_and_underscores() {
        assert!(is_valid_spin_key("foo"));
        assert!(is_valid_spin_key("foo_bar"));
        assert!(is_valid_spin_key("foo__bar"));
        assert!(is_valid_spin_key("a1b2"));
    }

    #[test]
    fn is_valid_spin_key_rejects_bad_starts_and_chars() {
        assert!(!is_valid_spin_key(""));
        assert!(!is_valid_spin_key("FOO"));
        assert!(!is_valid_spin_key("1foo"));
        assert!(!is_valid_spin_key("foo-bar"));
        assert!(!is_valid_spin_key("_foo"));
    }

    #[test]
    fn flatten_keys_walks_nested_tables_in_dotted_form() {
        let table: Table = toml::from_str(
            r#"
greeting = "hi"

[service]
timeout_ms = 1500

[service.inner]
deep = true
"#,
        )
        .expect("parse fixture");
        let mut keys = flatten_keys(&table);
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "greeting".to_owned(),
                "service.inner.deep".to_owned(),
                "service.timeout_ms".to_owned(),
            ]
        );
    }

    #[test]
    fn is_valid_handler_path_accepts_rust_path_shape() {
        assert!(is_valid_handler_path("app_demo_core::handlers::root"));
        assert!(is_valid_handler_path("crate::handler"));
        assert!(!is_valid_handler_path("not_a_path"));
        assert!(!is_valid_handler_path(""));
        assert!(!is_valid_handler_path("foo::1bar"));
    }
}
