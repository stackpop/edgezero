//! `config validate`.
//!
//! Two entry points share the same checks against the manifest, the
//! app-config file, and the per-adapter validators (`[component.*]`
//! discovery for Spin, etc.):
//!
//! - [`run_config_validate`] — raw flow. Loads the file's root
//!   table as a [`toml::Value`] only; the typed deserialise /
//!   `validator` / secret checks are skipped because no `C` is in
//!   scope. The default `edgezero` binary uses this.
//! - [`run_config_validate_typed`] — typed flow. Adds typed
//!   deserialisation, `validator::Validate::validate()`, and the
//!   `#[secret]` / `#[secret(store_ref)]` checks. Downstream project
//!   CLIs that own an app-config struct wire this up.
//!
//! Both run the manifest through [`ManifestLoader`] (which itself
//! validates everything) and apply the typed app-config's
//! env-overlay unless `--no-env` is passed, so the validation sees
//! the values the runtime would.

use crate::args::{ConfigPushArgs, ConfigValidateArgs};
use crate::ensure_adapter_defined;
use edgezero_adapter::registry::{self as adapter_registry, ResolvedStoreId};
use edgezero_core::app_config::{
    self, AppConfigError, AppConfigLoadOptions, AppConfigMeta, SecretKind,
};
use edgezero_core::env_config::EnvConfig;
use edgezero_core::manifest::{Manifest, ManifestLoader, StoreDeclaration};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use toml::value::Table;
use toml::Value;
use validator::Validate;

/// Pre-loaded state for either push flow. Shares
/// [`ValidationContext`] with the validate flows (manifest, raw
/// config, env-overlay-aware app-config path) and adds the resolved
/// target adapter + store id.
struct PushContext {
    adapter: &'static dyn adapter_registry::Adapter,
    /// Resolved push-time overlay values (seed URL / token / local
    /// flag). Owned so the lifetime story stays simple; `dispatch_push`
    /// borrows from this to build the `AdapterPushContext<'_>` it
    /// hands the trait method.
    adapter_push_ctx: ResolvedAdapterPushContext,
    /// Resolved config store id (`--store` or the manifest
    /// default), paired with its env-resolved platform name. The
    /// platform name is what the adapter writes / pushes into
    /// the per-platform backing (wrangler.toml binding, fastly
    /// config-store-name, spin variable space), so an operator
    /// who sets `EDGEZERO__STORES__CONFIG__<ID>__NAME=...` sees
    /// it honoured at push time, not just at runtime.
    store: ResolvedStoreId,
    /// Validate-shaped pre-loaded state (manifest + raw config).
    validation: ValidationContext,
}

/// Push-time overlay values resolved by [`load_push_context`].
/// Owned strings/paths so the CLI's borrow story stays simple —
/// `dispatch_push` creates the borrowing
/// [`adapter_registry::AdapterPushContext`] at the trait call boundary.
#[derive(Debug, Default)]
struct ResolvedAdapterPushContext {
    local: bool,
    runtime_config_path: Option<PathBuf>,
}

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
    /// overlay setting the typed flow will use, so the same
    /// flattened key set drives every adapter's `validate_*` call.
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
/// collision, which all require `AppConfigMeta`.
///
/// # Errors
/// Returns a human-readable error string on any validation failure.
#[inline]
pub fn run_config_validate(args: &ConfigValidateArgs) -> Result<(), String> {
    let ctx = load_validation_context(args)?;
    run_shared_checks(&ctx)?;
    log::info!(
        "[edgezero] config validate (raw): {} OK{}",
        args.manifest.display(),
        if args.strict { " (strict)" } else { "" },
    );
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
    run_adapter_typed_checks::<C>(&ctx)?;

    log::info!(
        "[edgezero] config validate (typed): {} + {} OK{}",
        args.manifest.display(),
        ctx.app_config_path.display(),
        if args.strict { " (strict)" } else { "" },
    );
    Ok(())
}

/// Raw flow — push the on-disk `<name>.toml` as a parsed TOML tree.
/// Skips no fields (no `SECRET_FIELDS` knowledge); the operator is
/// responsible for keeping sensitive material out of a raw push.
///
/// Spec: push runs strict pre-flight validation before writing
/// anything. For the raw flow that means the same shared checks
/// `config validate --strict` runs — adapter manifest discovery
/// (Spin component, etc.), per-adapter config-key validation
/// (Spin's `^[a-z][a-z0-9_]*$` rule), capability-aware
/// completeness (rejects multi-id stores against Single-capable
/// adapters), and handler-path well-formedness — not just the
/// schema/load-time checks. Skipping these would let a push
/// half-mutate a manifest before a key collision or a Single-
/// capable adapter rejected the entries downstream.
///
/// # Errors
/// Returns a human-readable error string on any load / resolution /
/// adapter-push failure.
#[inline]
pub fn run_config_push(args: &ConfigPushArgs) -> Result<(), String> {
    let ctx = load_push_context(args)?;
    run_shared_checks(&ctx.validation)?;
    let entries = flatten_raw_for_push(&ctx.validation.raw_config)?;
    dispatch_push(&ctx, &entries, args.dry_run, args.local)
}

/// Typed flow — push the user's `C` struct. Runs the same strict
/// pre-flight validation `config validate --strict` does (typed
/// deserialise, `validator::Validate`, secret checks), then
/// serialises `C` and feeds the adapter the flattened, dotted-key
/// entries with `SECRET_FIELDS` (any kind) stripped.
///
/// # Errors
/// Returns a human-readable error string on any validation or push
/// failure.
#[inline]
pub fn run_config_push_typed<C>(args: &ConfigPushArgs) -> Result<(), String>
where
    C: DeserializeOwned + Serialize + Validate + AppConfigMeta,
{
    let ctx = load_push_context(args)?;
    // Spec: strict pre-flight. The typed flow already runs
    // typed-only checks below; `run_shared_checks` here adds
    // everything `config validate --strict` does — shared
    // adapter checks (`[component.*]` discovery for Spin,
    // adapter-manifest well-formedness), capability-aware
    // completeness, and handler-path well-formedness. Without
    // this a Single-capable adapter with multi-id stores would
    // only surface inside the per-adapter push, potentially
    // after a partial mutation.
    run_shared_checks(&ctx.validation)?;

    let mut opts = AppConfigLoadOptions::default();
    opts.env_overlay = !args.no_env;
    let typed: C = app_config::load_app_config_with_options::<C>(
        &ctx.validation.app_config_path,
        &ctx.validation.app_name,
        &opts,
    )
    .map_err(|err| format_app_config_error(&err))?;

    typed
        .validate()
        .map_err(|err| format!("typed app-config failed validation: {err}"))?;
    typed_secret_checks(&typed, &ctx.validation)?;
    run_adapter_typed_checks::<C>(&ctx.validation)?;

    let entries = flatten_typed_for_push::<C>(&typed)?;
    dispatch_push(&ctx, &entries, args.dry_run, args.local)
}

// -------------------------------------------------------------------
// Push context + dispatch
// -------------------------------------------------------------------

fn load_push_context(args: &ConfigPushArgs) -> Result<PushContext, String> {
    // Spec: push is strict — the synthesized validate args
    // unconditionally request `--strict` so `run_shared_checks`
    // runs the capability-completeness + handler-path checks
    // alongside the schema and per-adapter shared checks.
    let validate_args = ConfigValidateArgs {
        app_config: args.app_config.clone(),
        manifest: args.manifest.clone(),
        no_env: args.no_env,
        strict: true,
    };
    let validation = load_validation_context(&validate_args)?;
    ensure_adapter_defined(&args.adapter, Some(&validation.manifest_loader))?;
    let adapter = adapter_registry::get_adapter(&args.adapter).ok_or_else(|| {
        format!(
            "adapter `{}` is declared in {} but not registered in this build (rebuild `edgezero-cli` with its feature enabled)",
            args.adapter,
            args.manifest.display()
        )
    })?;
    let logical = resolve_config_store_id(args.store.as_deref(), validation.manifest())?;
    let env_config = EnvConfig::from_env();
    let platform = env_config.store_name("config", &logical);
    let adapter_push_ctx =
        resolve_adapter_push_ctx(args, &env_config, validation.manifest(), &args.adapter);
    Ok(PushContext {
        adapter,
        adapter_push_ctx,
        store: ResolvedStoreId::new(logical, platform),
        validation,
    })
}

/// Resolve the push-time overlay values: `--local` flag (passed
/// through verbatim) and the adapter-runtime-config path (`--runtime-
/// config` flag if set; the adapter resolves a default location
/// otherwise).
fn resolve_adapter_push_ctx(
    args: &ConfigPushArgs,
    _env_config: &EnvConfig,
    _manifest: &Manifest,
    _adapter_name: &str,
) -> ResolvedAdapterPushContext {
    ResolvedAdapterPushContext {
        local: args.local,
        runtime_config_path: args.runtime_config.clone(),
    }
}

fn dispatch_push(
    ctx: &PushContext,
    entries: &[(String, String)],
    dry_run: bool,
    local: bool,
) -> Result<(), String> {
    let manifest = ctx.validation.manifest();
    let adapter_cfg = manifest.adapters.get(ctx.adapter.name()).ok_or_else(|| {
        format!(
            "adapter `{}` vanished from the manifest after lookup",
            ctx.adapter.name()
        )
    })?;
    let manifest_root = ctx
        .validation
        .manifest_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    // AdapterPushContext is #[non_exhaustive], so build it via the
    // builder API instead of a struct literal.
    let resolved = &ctx.adapter_push_ctx;
    let mut push_ctx = adapter_registry::AdapterPushContext::new().with_local(resolved.local);
    if let Some(path) = resolved.runtime_config_path.as_deref() {
        push_ctx = push_ctx.with_runtime_config_path(path);
    }
    if let Some(deploy_cmd) = adapter_cfg.commands.deploy.as_deref() {
        push_ctx = push_ctx.with_manifest_adapter_deploy_cmd(deploy_cmd);
    }
    let lines = if local {
        ctx.adapter.push_config_entries_local(
            manifest_root,
            adapter_cfg.adapter.manifest.as_deref(),
            adapter_cfg.adapter.component.as_deref(),
            &ctx.store,
            entries,
            &push_ctx,
            dry_run,
        )?
    } else {
        ctx.adapter.push_config_entries(
            manifest_root,
            adapter_cfg.adapter.manifest.as_deref(),
            adapter_cfg.adapter.component.as_deref(),
            &ctx.store,
            entries,
            &push_ctx,
            dry_run,
        )?
    };
    if dry_run {
        log::info!(
            "[edgezero] config push --dry-run{} for `{}` -> store `{}` (platform name `{}`):",
            if local { " --local" } else { "" },
            ctx.adapter.name(),
            ctx.store.logical,
            ctx.store.platform
        );
    }
    for line in lines {
        log::info!("{line}");
    }
    Ok(())
}

fn resolve_config_store_id(requested: Option<&str>, manifest: &Manifest) -> Result<String, String> {
    let Some(declaration) = manifest.stores.config.as_ref() else {
        return Err(
            "manifest has no `[stores.config]` section; declare it before pushing config"
                .to_owned(),
        );
    };
    if declaration.ids.is_empty() {
        return Err("[stores.config].ids is empty; declare at least one id".to_owned());
    }
    if let Some(name) = requested {
        if declaration.ids.iter().any(|id| id == name) {
            return Ok(name.to_owned());
        }
        return Err(format!(
            "--store={name:?} is not in [stores.config].ids ({:?})",
            declaration.ids
        ));
    }
    Ok(resolved_default(declaration))
}

fn resolved_default(declaration: &StoreDeclaration) -> String {
    declaration.default_id().to_owned()
}

// -------------------------------------------------------------------
// Flattening — raw (toml::Value) and typed (Serialize -> JSON)
// -------------------------------------------------------------------

fn flatten_raw_for_push(raw: &Value) -> Result<Vec<(String, String)>, String> {
    let json: serde_json::Value = serde_json::to_value(raw)
        .map_err(|err| format!("failed to convert raw TOML to JSON for flattening: {err}"))?;
    let mut out = Vec::new();
    flatten_json_into(&json, "", &BTreeSet::new(), &mut out)?;
    Ok(out)
}

fn flatten_typed_for_push<C>(typed: &C) -> Result<Vec<(String, String)>, String>
where
    C: Serialize + AppConfigMeta,
{
    let json = serde_json::to_value(typed)
        .map_err(|err| format!("failed to serialize typed app-config: {err}"))?;
    if !json.is_object() {
        return Err(
            "typed app-config did not serialize to a JSON object; only struct-shaped configs are supported"
                .to_owned(),
        );
    }
    // Skip every `#[secret]` AND `#[secret(store_ref)]` top-level
    // field — runtime store ids and secret values both belong out
    // of the config-store payload.
    let secret_field_names: BTreeSet<String> = C::SECRET_FIELDS
        .iter()
        .map(|field| field.name.to_owned())
        .collect();
    let mut out = Vec::new();
    flatten_json_into(&json, "", &secret_field_names, &mut out)?;
    Ok(out)
}

fn flatten_json_into(
    value: &serde_json::Value,
    prefix: &str,
    skip_top_level: &BTreeSet<String>,
    out: &mut Vec<(String, String)>,
) -> Result<(), String> {
    match value {
        serde_json::Value::Null => Ok(()),
        serde_json::Value::Bool(boolean) => {
            out.push((prefix.to_owned(), boolean.to_string()));
            Ok(())
        }
        serde_json::Value::Number(number) => {
            out.push((prefix.to_owned(), number.to_string()));
            Ok(())
        }
        serde_json::Value::String(text) => {
            out.push((prefix.to_owned(), text.clone()));
            Ok(())
        }
        serde_json::Value::Array(_) => {
            //: arrays are JSON-encoded into a single value.
            let encoded = serde_json::to_string(value)
                .map_err(|err| format!("failed to JSON-encode array at key `{prefix}`: {err}"))?;
            out.push((prefix.to_owned(), encoded));
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if prefix.is_empty() && skip_top_level.contains(key) {
                    continue;
                }
                let full = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                // Only the top-level skip-set applies; nested keys
                // can never be secrets (SECRET_FIELDS is top-level).
                flatten_json_into(child, &full, &BTreeSet::new(), out)?;
            }
            Ok(())
        }
    }
}

fn load_validation_context(args: &ConfigValidateArgs) -> Result<ValidationContext, String> {
    let manifest_loader = ManifestLoader::from_path(&args.manifest)
        .map_err(|err| format!("failed to load {}: {err}", args.manifest.display()))?;

    // Spec: every project carries a `[app].name`. Without it we
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
    // validator; we keep this copy for shared checks (e.g. Spin
    // `[component.*]` discovery) that don't need `C`.
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
    run_adapter_shared_checks(ctx)?;
    if ctx.args_strict {
        strict_capability_completeness(ctx.manifest())?;
        strict_handler_paths(ctx.manifest())?;
    }
    Ok(())
}

// -------------------------------------------------------------------
// Adapter dispatch — defer per-adapter rules to each adapter crate's
// `Adapter` trait impl. No `if adapter == "spin"` branches here.
// -------------------------------------------------------------------

/// Run the adapter-agnostic shared checks: for every adapter
/// declared in the manifest, look up its `Adapter` impl in the
/// registry and invoke `validate_app_config_keys` +
/// `validate_adapter_manifest`. Adapters not in the registry (e.g.
/// a feature-gated build that omitted some) are silently skipped —
/// they can't validate what they don't link.
fn run_adapter_shared_checks(ctx: &ValidationContext) -> Result<(), String> {
    let raw_table = ctx
        .raw_config
        .as_table()
        .ok_or_else(|| "raw app-config was not a TOML table after load".to_owned())?;
    let flattened = flatten_keys(raw_table);
    let key_refs: Vec<&str> = flattened.iter().map(String::as_str).collect();
    let manifest_root = ctx.manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let env_config = EnvConfig::from_env();

    for (name, adapter_cfg) in &ctx.manifest().adapters {
        let Some(adapter) = adapter_registry::get_adapter(name) else {
            continue;
        };
        adapter.validate_app_config_keys(&key_refs)?;
        adapter.validate_adapter_manifest(
            manifest_root,
            adapter_cfg.adapter.manifest.as_deref(),
            adapter_cfg.adapter.component.as_deref(),
        )?;
        reject_merged_id_collisions(name, adapter, ctx.manifest(), &env_config)?;
    }
    Ok(())
}

/// Reject overlap across store kinds the adapter merges into a single
/// backend (e.g. Spin's KV + Config both back to
/// `spin_sdk::key_value::Store`). Checks BOTH logical-id collisions
/// (`[stores.kv].ids = ["x"]` + `[stores.config].ids = ["x"]`) AND
/// env-resolved platform-label collisions (different logical ids
/// pointing at the same platform label via
/// `EDGEZERO__STORES__<KIND>__<ID>__NAME=shared`). Without the
/// platform-label half, an operator can use the env overlay to
/// silently route two logically-distinct stores to the same
/// underlying KV label at runtime.
pub(crate) fn reject_merged_id_collisions(
    adapter_name: &str,
    adapter: &'static dyn adapter_registry::Adapter,
    manifest: &Manifest,
    env_config: &EnvConfig,
) -> Result<(), String> {
    let merged = adapter.merged_id_kinds();
    if merged.len() < 2 {
        return Ok(());
    }
    // (id -> first-seen-kind) tracks logical-id collisions.
    // (platform_label -> (kind, id)) tracks env-resolved platform-label
    // collisions across kinds.
    let mut seen_ids: BTreeMap<&str, &str> = BTreeMap::new();
    let mut seen_platform: BTreeMap<String, (&str, String)> = BTreeMap::new();
    for kind in merged {
        let maybe_decl = match *kind {
            "kv" => manifest.stores.kv.as_ref(),
            "config" => manifest.stores.config.as_ref(),
            "secrets" => manifest.stores.secrets.as_ref(),
            _ => continue,
        };
        let Some(decl) = maybe_decl else {
            continue;
        };
        for id in &decl.ids {
            if let Some(prior_kind) = seen_ids.insert(id.as_str(), kind) {
                return Err(format!(
                    "logical id `{id}` is declared under BOTH `[stores.{prior_kind}]` and `[stores.{kind}]`, but adapter `{adapter_name}` backs those kinds with the same runtime store. Rename one -- the two would silently share writes via the same `key_value_stores` label.",
                ));
            }

            // Resolve the platform label per env: looks up
            // EDGEZERO__STORES__<KIND>__<ID>__NAME, falling back to
            // the logical id. Two distinct logical ids that resolve
            // to the same platform label across merged kinds collide
            // at runtime exactly the same way as a logical-id
            // collision — silently shared writes.
            let platform = env_config.store_name(kind, id);
            if let Some((prior_kind, prior_id)) =
                seen_platform.insert(platform.clone(), (kind, id.clone()))
            {
                if prior_kind != *kind || prior_id != *id {
                    return Err(format!(
                        "stores `[stores.{prior_kind}].{prior_id}` and `[stores.{kind}].{id}` both resolve to platform label `{platform}` (via the `EDGEZERO__STORES__<KIND>__<ID>__NAME` overlay or matching logical-id default), but adapter `{adapter_name}` backs those kinds with the same runtime store. Renaming one of the env overrides (or removing them so the logical ids stay distinct) fixes this -- both writes currently land on the same `key_value_stores` label.",
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Typed-only adapter dispatch: feed each adapter the `#[secret]`
/// (`KeyInDefault` only — `#[secret(store_ref)]` values are runtime
/// store ids, not flat-namespace candidates) so adapters whose
/// secret store has a flat-namespace constraint (Spin) can detect
/// within-secrets collisions.
fn run_adapter_typed_checks<C: AppConfigMeta>(ctx: &ValidationContext) -> Result<(), String> {
    let raw_table = ctx
        .raw_config
        .as_table()
        .ok_or_else(|| "raw app-config was not a TOML table after load".to_owned())?;

    let mut plain_secrets: Vec<(&str, &str)> = Vec::new();
    for field in C::SECRET_FIELDS {
        if !matches!(field.kind, SecretKind::KeyInDefault) {
            continue;
        }
        if let Some(value) = raw_table.get(field.name).and_then(Value::as_str) {
            plain_secrets.push((field.name, value));
        }
    }

    for name in ctx.manifest().adapters.keys() {
        if let Some(adapter) = adapter_registry::get_adapter(name) {
            adapter.validate_typed_secrets(&plain_secrets)?;
        }
    }
    Ok(())
}

// -------------------------------------------------------------------
// Typed secret checks
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
// flatten_keys — produces the dotted-path inventory each adapter
// trait method consumes. Lives here because it's the CLI's
// responsibility to walk the parsed TOML; adapters work with the
// already-flattened slice.
// -------------------------------------------------------------------

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

// -------------------------------------------------------------------
// --strict checks
// -------------------------------------------------------------------

fn strict_capability_completeness(manifest: &Manifest) -> Result<(), String> {
    // Spec capability matrix, driven by each adapter crate's
    // `Adapter::single_store_kinds()` impl. Adapters not in the
    // registry (e.g. a feature-gated build that omitted some) are
    // skipped — we can't speak for what isn't linked.
    for adapter_name in manifest.adapters.keys() {
        enforce_single_store_capability(manifest, adapter_name)?;
    }
    Ok(())
}

/// Per-adapter capability check shared by `config validate --strict`
/// (which iterates over every declared adapter) and `provision` /
/// `config push` (which target a single adapter). Surfaces a clear
/// error when the manifest declares more ids for a store kind than
/// the adapter can model. An unregistered adapter is a no-op --
/// we can't speak for what isn't linked into this build.
pub(crate) fn enforce_single_store_capability(
    manifest: &Manifest,
    adapter_name: &str,
) -> Result<(), String> {
    let Some(adapter) = adapter_registry::get_adapter(adapter_name) else {
        return Ok(());
    };
    let single_kinds = adapter.single_store_kinds();
    if single_kinds.is_empty() {
        return Ok(());
    }
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
        if single_kinds.contains(&kind) {
            return Err(format!(
                "adapter `{adapter_name}` is Single-capable for {kind} stores but [stores.{kind}].ids declares {} ids; pick one or drop the adapter",
                declaration.ids.len()
            ));
        }
    }
    Ok(())
}

pub(crate) fn strict_handler_paths(manifest: &Manifest) -> Result<(), String> {
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
    use crate::test_support::EnvOverride;
    use edgezero_core::app_config::SecretField;
    use serde::{Deserialize, Serialize};
    use std::fs;
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

    /// `[stores.config]` is required for push; the validate
    /// fixtures don't declare it. This fixture is push-shaped: axum
    /// adapter + a single config store id + a secrets section so
    /// the typed flow's `#[secret]` checks pass.
    const PUSH_MANIFEST: &str = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-app-adapter-axum"

[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

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
source = "target/wasm32-wasip2/release/demo.wasm"
"#;

    /// `AppDemoConfig`-shaped fixture: `greeting` + `api_token` (a
    /// `#[secret]`) + `vault` (a `#[secret(store_ref)]`) + nested
    /// `service`. Fields are read through Serialize (typed-push
    /// tests) and validator (`#[validate(...)]`), which is why this
    /// no longer needs a `dead_code` allow.
    #[derive(Debug, Deserialize, Serialize, Validate)]
    #[serde(deny_unknown_fields)]
    struct FixtureConfig {
        api_token: String,
        #[validate(length(min = 1_u64))]
        greeting: String,
        #[validate(nested)]
        service: FixtureServiceConfig,
        vault: String,
    }

    #[derive(Debug, Deserialize, Serialize, Validate)]
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

    fn push_args(manifest: &Path, adapter: &str) -> ConfigPushArgs {
        ConfigPushArgs {
            adapter: adapter.to_owned(),
            app_config: None,
            dry_run: false,
            local: false,
            manifest: manifest.to_path_buf(),
            no_env: true,
            runtime_config: None,
            store: None,
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

    // ---------- Spin checks ----------

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
    fn spin_logical_id_collision_across_kv_and_config_is_rejected() {
        // Spin merges KV + Config into one `key_value::Store` per
        // label. Declaring `sessions` under BOTH kinds resolves to
        // one underlying store; the runtime would silently share
        // writes between `kv_store("sessions")` and
        // `config_store("sessions")`. Validator catches.
        let manifest_str = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.kv]
ids = ["sessions"]

[stores.config]
ids = ["sessions"]

[stores.secrets]
ids = ["default"]
"#;
        let (dir, manifest, _) = setup_project(manifest_str, VALID_APP_CONFIG);
        write_spin_toml(dir.path(), VALID_SPIN_TOML);
        let err = run_config_validate(&args_for(&manifest))
            .expect_err("merged-kind id collision must error");
        assert!(
            err.contains("sessions")
                && err.contains("[stores.kv]")
                && err.contains("[stores.config]"),
            "error names the colliding id + both kinds: {err}"
        );
    }

    #[test]
    fn spin_distinct_logical_ids_collide_when_env_overlay_resolves_to_same_platform_label() {
        use crate::test_support::manifest_guard;
        // F2: distinct logical ids `sessions` (KV) and `app_config`
        // (Config) BOTH map to the same Spin KV label via the env
        // overlay. The runtime opens one underlying store for both
        // -- silent shared writes. The merged-id check must catch
        // this even though the logical ids differ.
        let manifest_str = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.kv]
ids = ["sessions"]

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        let (dir, manifest, _) = setup_project(manifest_str, VALID_APP_CONFIG);
        write_spin_toml(dir.path(), VALID_SPIN_TOML);

        // Serialise tests that touch process-global env state per
        // `crate::test_support` docs.
        let _lock = manifest_guard().lock().expect("manifest guard");
        // Both stores forced onto the platform label `shared`.
        let _kv_override = EnvOverride::set("EDGEZERO__STORES__KV__SESSIONS__NAME", "shared");
        let _config_override =
            EnvOverride::set("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME", "shared");

        let mut args = args_for(&manifest);
        args.no_env = false; // we WANT the env overlay here
        let err = run_config_validate(&args)
            .expect_err("platform-label collision via env overlay must fail validation");
        assert!(
            err.contains("`shared`")
                && err.contains("[stores.kv].sessions")
                && err.contains("[stores.config].app_config"),
            "error names the resolved platform label + both logical ids: {err}"
        );
    }

    #[test]
    fn spin_distinct_logical_ids_across_kv_and_config_validate_cleanly() {
        // Sanity: distinct ids across the merged kinds are fine.
        let manifest_str = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.kv]
ids = ["sessions"]

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        let (dir, manifest, _) = setup_project(manifest_str, VALID_APP_CONFIG);
        write_spin_toml(dir.path(), VALID_SPIN_TOML);
        run_config_validate(&args_for(&manifest))
            .expect("distinct ids across kinds must validate cleanly");
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

    // -------------------------------------------------------------------
    // config push (raw + typed) — spec
    // -------------------------------------------------------------------

    // ---------- raw push ----------

    #[test]
    fn raw_push_axum_writes_local_config_json() {
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, VALID_APP_CONFIG);
        run_config_push(&push_args(&manifest, "axum")).expect("push succeeds");
        let written = dir.path().join(".edgezero/local-config-app_config.json");
        let raw = fs::read_to_string(&written).expect("wrote local-config file");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        // The raw flow doesn't strip anything — `api_token` from
        // VALID_APP_CONFIG appears in the payload.
        assert_eq!(parsed["api_token"], "demo_api_token");
        assert_eq!(parsed["greeting"], "hello");
    }

    #[test]
    fn raw_push_dry_run_does_not_write() {
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, VALID_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.dry_run = true;
        run_config_push(&args).expect("dry-run succeeds");
        assert!(
            !dir.path().join(".edgezero").exists(),
            ".edgezero must not be created in dry-run"
        );
    }

    #[test]
    fn raw_push_errors_when_stores_config_missing() {
        let manifest_no_config = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo"

[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#;
        let (_dir, manifest, _) = setup_project(manifest_no_config, VALID_APP_CONFIG);
        let err = run_config_push(&push_args(&manifest, "axum"))
            .expect_err("missing [stores.config] must error");
        assert!(
            err.contains("[stores.config]"),
            "error names the missing section: {err}"
        );
    }

    #[test]
    fn raw_push_errors_when_adapter_not_declared() {
        let (_dir, manifest, _) = setup_project(PUSH_MANIFEST, VALID_APP_CONFIG);
        let err = run_config_push(&push_args(&manifest, "not-an-adapter"))
            .expect_err("undeclared adapter must error");
        assert!(
            err.contains("not-an-adapter"),
            "error names the undeclared adapter: {err}"
        );
    }

    #[test]
    fn raw_push_respects_explicit_store_selection() {
        // Two declared ids — push to the non-default one via --store.
        let manifest_two_ids = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo"

[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config", "alt_config"]
default = "app_config"

[stores.secrets]
ids = ["default"]
"#;
        let (dir, manifest, _) = setup_project(manifest_two_ids, VALID_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.store = Some("alt_config".to_owned());
        run_config_push(&args).expect("explicit --store=alt_config succeeds");
        assert!(
            dir.path()
                .join(".edgezero/local-config-alt_config.json")
                .exists(),
            "push targeted alt_config"
        );
        assert!(
            !dir.path()
                .join(".edgezero/local-config-app_config.json")
                .exists(),
            "default store untouched"
        );
    }

    #[test]
    fn raw_push_rejects_unknown_store_id() {
        let (_dir, manifest, _) = setup_project(PUSH_MANIFEST, VALID_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.store = Some("does-not-exist".to_owned());
        let err = run_config_push(&args).expect_err("bad --store must error");
        assert!(
            err.contains("does-not-exist"),
            "error names the bad store id: {err}"
        );
    }

    #[test]
    fn raw_push_resolves_default_from_multi_id_store() {
        // [stores.config].ids = ["one", "two"], default = "two".
        let manifest_with_default = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo"

[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["one", "two"]
default = "two"

[stores.secrets]
ids = ["default"]
"#;
        let (dir, manifest, _) = setup_project(manifest_with_default, VALID_APP_CONFIG);
        run_config_push(&push_args(&manifest, "axum")).expect("default resolves to `two`");
        assert!(
            dir.path().join(".edgezero/local-config-two.json").exists(),
            "push targeted the `default` id"
        );
    }

    #[test]
    fn raw_push_flattens_nested_tables_into_dotted_keys() {
        let app_config = r#"
greeting = "hi"

[service]
timeout_ms = 1500
"#;
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, app_config);
        run_config_push(&push_args(&manifest, "axum")).expect("push succeeds");
        let raw = fs::read_to_string(dir.path().join(".edgezero/local-config-app_config.json"))
            .expect("read written file");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(parsed["greeting"], "hi");
        assert_eq!(
            parsed["service.timeout_ms"], "1500",
            "nested field flattened to dotted key + stringified: {parsed}"
        );
    }

    #[test]
    fn raw_push_json_encodes_arrays() {
        //: arrays become a single JSON-encoded string value.
        let app_config = "tags = [\"a\", \"b\", \"c\"]\n";
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, app_config);
        run_config_push(&push_args(&manifest, "axum")).expect("push succeeds");
        let raw = fs::read_to_string(dir.path().join(".edgezero/local-config-app_config.json"))
            .expect("read written file");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(parsed["tags"], "[\"a\",\"b\",\"c\"]");
    }

    // ---------- non-axum adapters (dry-run dispatch tests) ----------

    #[test]
    fn raw_push_cloudflare_dry_run_dispatches_to_adapter() {
        // Real impl shipped in 7.2 — dry-run resolves the namespace
        // id from wrangler.toml but doesn't shell out, so CI can
        // exercise dispatch without wrangler installed.
        let manifest_cf = r#"
[app]
name = "demo-app"

[adapters.cloudflare.adapter]
crate = "crates/demo-cf"
manifest = "wrangler.toml"

[adapters.cloudflare.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        let (dir, manifest, _) = setup_project(manifest_cf, VALID_APP_CONFIG);
        // The adapter resolves wrangler.toml relative to the
        // manifest root and reads the namespace id by binding —
        // write one so dispatch reaches the dry-run branch.
        fs::write(
            dir.path().join("wrangler.toml"),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"abc123\"\n",
        )
        .expect("write wrangler.toml");
        let mut args = push_args(&manifest, "cloudflare");
        args.dry_run = true;
        run_config_push(&args).expect("cloudflare dry-run dispatches cleanly");
    }

    #[test]
    fn raw_push_fastly_dry_run_dispatches_to_adapter() {
        // Real impl shipped in 7.3 — dry-run skips the `fastly
        // config-store list --json` resolver and the per-entry
        // create shell-out, so CI exercises dispatch without
        // fastly on PATH.
        let manifest_fastly = r#"
[app]
name = "demo-app"

[adapters.fastly.adapter]
crate = "crates/demo-fastly"
manifest = "fastly.toml"

[adapters.fastly.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        let (_dir, manifest, _) = setup_project(manifest_fastly, VALID_APP_CONFIG);
        let mut args = push_args(&manifest, "fastly");
        args.dry_run = true;
        run_config_push(&args).expect("fastly dry-run dispatches cleanly");
    }

    #[test]
    fn raw_push_spin_dry_run_dispatches_to_adapter() {
        // The CLI must thread `args.local` + `--runtime-config` + the
        // manifest's `[adapters.spin.commands].deploy` through to
        // Spin's `push_config_entries`, where the per-backend
        // dispatcher decides whether to write SQLite-direct or shell
        // to Fermyon Cloud. End-to-end: a raw dry-run with
        // `args.local = true` against a fixture project should hit
        // Spin's `dispatch_push` and announce a SQLite-direct write
        // even with no `runtime-config.toml` present (default
        // branch). The deeper per-backend matrix lives in spin
        // adapter's `dispatch_push_*` tests; this one is the CLI
        // wiring regression.
        let manifest_spin = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        let (dir, manifest, _) = setup_project(manifest_spin, VALID_APP_CONFIG);
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        )
        .expect("write spin.toml");
        // Non-default labels (here `app_config`) require a
        // runtime-config stanza; otherwise the dispatcher errors
        // early to keep the runtime invariant honest.
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");
        let mut args = push_args(&manifest, "spin");
        args.dry_run = true;
        args.local = true;
        run_config_push(&args).expect("spin --local dry-run dispatches cleanly");
        // Spin.toml must be byte-identical after the dispatch — the
        // per-backend writer NEVER edits it (the seed-handler-era
        // `[variables]` writes are gone for good).
        let spin_toml = fs::read_to_string(dir.path().join("spin.toml")).expect("re-read");
        assert!(
            spin_toml.contains("[component.demo]") && !spin_toml.contains("[variables]"),
            "dispatcher must not touch spin.toml: {spin_toml}"
        );
    }

    // ---------- typed push ----------

    #[test]
    fn typed_push_strips_secret_fields_from_payload() {
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        // FixtureConfig requires `api_token` (#[secret]) and `vault`
        // (#[secret(store_ref)]) — both should be absent from the
        // pushed payload.
        args.app_config = Some(dir.path().join("demo-app.toml"));
        run_config_push_typed::<FixtureConfig>(&args).expect("typed push succeeds");
        let raw = fs::read_to_string(dir.path().join(".edgezero/local-config-app_config.json"))
            .expect("read written file");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert!(
            parsed.get("api_token").is_none(),
            "#[secret] field must be stripped: {parsed}"
        );
        assert!(
            parsed.get("vault").is_none(),
            "#[secret(store_ref)] field must be stripped: {parsed}"
        );
        assert_eq!(parsed["greeting"], "hello");
        assert_eq!(parsed["service.timeout_ms"], "1500");
    }

    #[test]
    fn typed_push_runs_validator_and_errors_on_bad_config() {
        let bad_config = r#"
api_token = "demo"
greeting = "hi"
vault = "default"

[service]
timeout_ms = 50
"#;
        let (_dir, manifest, _) = setup_project(PUSH_MANIFEST, bad_config);
        let err = run_config_push_typed::<FixtureConfig>(&push_args(&manifest, "axum"))
            .expect_err("validator failure must abort push");
        assert!(
            err.to_lowercase().contains("validation"),
            "error names validation: {err}"
        );
    }

    #[test]
    fn typed_push_dry_run_does_not_write() {
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.dry_run = true;
        run_config_push_typed::<FixtureConfig>(&args).expect("dry-run succeeds");
        assert!(
            !dir.path().join(".edgezero").exists(),
            ".edgezero must not be created in dry-run"
        );
    }

    // ---------- push runs the strict preflight (regression) ----------

    /// Push must run the same shared adapter checks `config
    /// validate` runs (spec strict pre-flight). Pre-fix,
    /// `load_push_context` synthesised `ConfigValidateArgs { strict:
    /// false }` and `run_config_push*` never called
    /// `run_shared_checks`, so an adapter-specific shape error
    /// only surfaced inside `Adapter::push_config_entries` —
    /// risking a partial mutation in any future adapter without
    /// the same belt-and-braces guard. We probe via Spin's
    /// `validate_adapter_manifest`, which fails when the
    /// referenced spin.toml has no `[component.*]` declarations.
    #[test]
    fn raw_push_runs_spin_adapter_manifest_check_before_push() {
        let app_config = r#"
api_token = "x"
greeting = "hi"
"#;
        let manifest_spin = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        let (dir, manifest, _) = setup_project(manifest_spin, app_config);
        // spin.toml with ZERO components — Spin's
        // validate_adapter_manifest must reject before the per-
        // adapter push gets a chance to mutate anything.
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n",
        )
        .expect("write spin.toml");
        let err = run_config_push(&push_args(&manifest, "spin"))
            .expect_err("missing [component.*] must fail Spin's shared-check preflight");
        assert!(
            err.contains("no [component.*]") || err.contains("component"),
            "error must come from Spin's shared validate_adapter_manifest: {err}"
        );
    }

    #[test]
    fn typed_push_runs_strict_capability_completeness_before_push() {
        // Spin is Single-capable for `[stores.secrets]`;
        // declaring two ids is a `--strict` capability violation
        // that the typed push must catch before invoking the
        // adapter.
        let manifest_strict_violation = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"
[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["one", "two"]
default = "one"
"#;
        let (dir, manifest, _) = setup_project(manifest_strict_violation, FIXTURE_APP_CONFIG);
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"a.wasm\"\n",
        )
        .expect("write spin.toml");
        // Adapter the push targets doesn't matter — the strict
        // capability check fires per declared adapter set. We
        // push to axum to keep the rest of the flow simple.
        let err = run_config_push_typed::<FixtureConfig>(&push_args(&manifest, "axum"))
            .expect_err("Single-capable adapter with multi-id store must fail preflight");
        // BTreeMap iteration order on the manifest's adapter set
        // means the check reports whichever Single-capable
        // adapter sorts first (axum or spin) — both are
        // Single-capable for secrets in this fixture. The
        // contract that matters is "the strict check ran before
        // the per-adapter push", which the `Single` +
        // `secrets` substrings prove.
        assert!(
            err.contains("Single") && err.contains("secrets"),
            "error must come from --strict capability check: {err}"
        );
    }
}
