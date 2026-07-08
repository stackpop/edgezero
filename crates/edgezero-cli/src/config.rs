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

use crate::args::{ConfigDiffArgs, ConfigPushArgs, ConfigValidateArgs, DiffFormat};
use crate::diff::{collect_changes, render_json, render_structured};
use crate::ensure_adapter_defined;
use edgezero_adapter::registry::{
    self as adapter_registry, ReadConfigEntry, ResolvedStoreId, TypedSecretEntry,
};
use edgezero_core::app_config::{
    self, AppConfigError, AppConfigLoadOptions, AppConfigMeta, SecretField, SecretKind,
    SecretPathSegment,
};
use edgezero_core::blob_envelope::BlobEnvelope;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::manifest::{Manifest, ManifestLoader, StoreDeclaration};
use serde::de::DeserializeOwned;
use serde::Serialize;
use similar::TextDiff;
use std::collections::BTreeMap;
use std::io::{stdin, Error as IoError, IsTerminal as _, Write};
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
    /// flag). Owned so the lifetime story stays simple; the typed-push
    /// helper borrows from this to build the `AdapterPushContext<'_>`
    /// it hands the adapter trait method.
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
/// the typed-push helper creates the borrowing
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

// -------------------------------------------------------------------
// Types (must precede all fns per clippy::arbitrary_source_item_ordering)
// -------------------------------------------------------------------

/// Typed exit-code outcome of a successful `config diff` run.
///
/// Returned so the generated CLI's main can pick the right exit code
/// per Q10.  NOT used on the error path — `Result::Err` bypasses this
/// and the main always exits ≥ 2.
///
/// Exit code semantics (Q10):
/// - `0` — no changes, or `--exit-code` is false.
/// - `1` — diff present AND `--exit-code` was set (CI gate signal).
/// - `2` — diff structurally impossible (`Unsupported`).
#[derive(Debug)]
pub struct DiffExit {
    /// 0 (no changes), 1 (diff present with `--exit-code`), or 2 (Unsupported).
    pub code: i32,
}

/// Internal outcome of a `config diff` run. Drives `apply_exit_code`.
/// Variants are alphabetical per `clippy::arbitrary_source_item_ordering`.
enum DiffOutcome {
    /// Diff is present (remote != local, or remote absent).
    DiffPresent,
    /// SHA matched — no changes.
    NoChanges,
    /// Remote was absent (`MissingKey` or `MissingStore`). Treated as
    /// "all leaves added" — rendered as a diff but reported separately
    /// so `--exit-code` can still signal "something would change".
    RemoteAbsent,
    /// Adapter does not support remote read-back (e.g. Spin Cloud).
    /// The `reason` string is printed to stderr before this variant is
    /// constructed, so the field itself is not read here.
    Unsupported,
}

/// Outcome of the first read + diff render step.
enum FirstReadOutcome {
    /// Remote SHA == local SHA; caller returns `Ok(())` immediately.
    NoChange,
    /// Remote was Missing or Unsupported; nothing to compare against,
    /// caller proceeds to consent.
    ProceedFromMissingOrUnsupported,
    /// Remote was `Present` with a different SHA; carry the SHA forward so
    /// The pre-write re-fetch can race-detect against it.
    ProceedFromPresent { approved_remote_sha: String },
}

/// Borrowed adapter paths threaded through the typed-push helpers.
/// All fields are borrowed so the struct is `Copy`-cheap.
struct PushPathRefs<'pp> {
    adapter_manifest_path: Option<&'pp str>,
    component_selector: Option<&'pp str>,
    manifest_root: &'pp Path,
    push_ctx: &'pp adapter_registry::AdapterPushContext<'pp>,
}

/// Outcome of the pre-write re-check.
enum RecheckOutcome {
    /// Concurrent push reached the same state — skip the write.
    Skip,
    /// Proceed to write (any warnings already emitted).
    Write,
}

/// One resolved `#[secret]` leaf located in the raw app-config TOML.
///
/// `label` carries concrete `[n]` array indices (the runtime dotted
/// form used in CLI output and errors). `store_ref_value` is the sibling
/// store id resolved from the leaf's INNERMOST parent table — populated
/// only for `KeyInNamedStore` leaves.
#[derive(Debug)]
struct ResolvedTomlLeaf<'raw> {
    /// Dotted runtime label, e.g. `partners[1].api_key`.
    label: String,
    /// Sibling store id for `KeyInNamedStore`; `None` otherwise.
    store_ref_value: Option<&'raw str>,
    /// The secret leaf's string value (a secret-store KEY NAME).
    value: &'raw str,
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

    // Typed deserialise + validate_excluding_secrets (spec 3.3.8: push,
    // diff, AND typed validate all use deserialize-only +
    // validate_excluding_secrets; the runtime is the only path that runs
    // full validate against RESOLVED secret values).
    let mut opts = AppConfigLoadOptions::default();
    opts.env_overlay = !args.no_env;
    let typed: C = app_config::deserialize_app_config_with_options::<C>(
        &ctx.app_config_path,
        &ctx.app_name,
        &opts,
    )
    .map_err(|err| format_app_config_error(&err))?;
    app_config::validate_excluding_secrets(&typed)
        .map_err(|err| format!("typed app-config failed validation: {err}"))?;

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

// -------------------------------------------------------------------
/// Stub-pointer for the bundled `edgezero` binary's `config push`
/// subcommand (spec 3.2.1).
///
/// The blob app-config rewrite requires a TYPED downstream CLI that
/// embeds the app's `AppConfig<C>` struct. The bundled `edgezero`
/// binary has no such type in scope, so `config push` on the bundled
/// binary is intentionally unsupported in v1. Downstream projects
/// generate their own `<app-name>-cli` binary that calls
/// [`run_config_push_typed`] with their concrete `C`.
///
/// This function always returns `Err(...)` with a pointer to the
/// typed downstream CLI and the spec section. The subcommand itself
/// must still be registered in the bundled binary's `Command` enum so
/// `edgezero config push --help` is reachable and the error message
/// can be displayed.
///
/// # Errors
/// Always returns a pointer error explaining 3.2.1.
#[inline]
pub fn run_config_push(_args: &ConfigPushArgs) -> Result<(), String> {
    Err(
        "`config push` on the bundled `edgezero` binary is not supported in v1; \
         the blob app-config rewrite requires the typed downstream CLI \
         (e.g. `<app-name>-cli config push`) which embeds your typed \
         AppConfig<C>. See docs/superpowers/specs/2026-06-16-blob-app-config.md \u{00a7}3.2.1."
            .to_owned(),
    )
}

/// Typed flow — push the user's `C` struct. Runs strict pre-flight
/// validation, then builds a `BlobEnvelope`, reads back the current
/// remote for skip-on-equal + inline diff, prompts for consent, and
/// writes via the adapter.
///
/// # Errors
/// Returns a human-readable error string on any validation, read-back,
/// consent, or push failure.
#[inline]
pub fn run_config_push_typed<C>(args: &ConfigPushArgs) -> Result<(), String>
where
    C: DeserializeOwned + Serialize + Validate + AppConfigMeta,
{
    // Pre-flight: load + validate.
    let ctx = load_push_context(args)?;
    run_shared_checks(&ctx.validation)?;
    let mut opts = AppConfigLoadOptions::default();
    opts.env_overlay = !args.no_env;
    let typed: C = app_config::deserialize_app_config_with_options::<C>(
        &ctx.validation.app_config_path,
        &ctx.validation.app_name,
        &opts,
    )
    .map_err(|err| format_app_config_error(&err))?;
    app_config::validate_excluding_secrets(&typed)
        .map_err(|err| format!("typed app-config failed validation: {err}"))?;
    typed_secret_checks(&typed, &ctx.validation)?;
    run_adapter_typed_checks::<C>(&ctx.validation)?;

    // Resolve adapter paths.
    let (manifest_root, adapter_manifest_path, component_selector, push_ctx) =
        resolve_push_paths(&ctx)?;
    let paths = PushPathRefs {
        manifest_root,
        adapter_manifest_path: adapter_manifest_path.as_deref(),
        component_selector: component_selector.as_deref(),
        push_ctx: &push_ctx,
    };

    // Build envelope.
    // Honour --key override (5.4): if the caller supplied an explicit key,
    // use it; otherwise fall back to the manifest's resolved logical store id.
    let key = args
        .key
        .clone()
        .unwrap_or_else(|| ctx.store.logical.clone());
    let body = build_config_envelope::<C>(&typed)?;
    let local_envelope: BlobEnvelope =
        serde_json::from_str(&body).map_err(|err| format!("local envelope parse failed: {err}"))?;
    let local_sha = local_envelope.sha256.clone();

    // First read + diff.
    let remote = read_remote(ctx.adapter, args.local, &paths, &ctx.store, &key)?;
    let approved_remote_sha =
        match render_first_read_diff(&remote, &key, &local_envelope, &local_sha, args.no_diff)? {
            FirstReadOutcome::NoChange => {
                push_info(&format!("# no changes (sha256 matches: {local_sha})"));
                return Ok(());
            }
            FirstReadOutcome::ProceedFromPresent {
                approved_remote_sha,
            } => Some(approved_remote_sha),
            FirstReadOutcome::ProceedFromMissingOrUnsupported => None,
        };

    // Consent gate (8.2 default or 8.3 Spin Cloud Unsupported).
    handle_consent(args, &remote)?;

    // Pre-write re-fetch + skip-on-equal + concurrent-push detection.
    if !args.dry_run && !matches!(remote, ReadConfigEntry::Unsupported(_)) {
        match recheck_before_write(
            ctx.adapter,
            args,
            &paths,
            &ctx.store,
            &key,
            &local_sha,
            &remote,
            approved_remote_sha.as_deref(),
        )? {
            RecheckOutcome::Skip => return Ok(()),
            RecheckOutcome::Write => {}
        }
    }

    // Write.
    write_envelope(ctx.adapter, args, &ctx, &paths, &key, body)
}

// -------------------------------------------------------------------
// run_config_diff_typed — typed diff entry point
// -------------------------------------------------------------------

/// Write a diff informational message to stderr.
/// All non-error diff messages go here rather than using inline `#[expect]` blocks.
#[expect(
    clippy::print_stderr,
    reason = "stream discipline: informational messages go to stderr, never stdout"
)]
fn diff_info(msg: &str) {
    eprintln!("{msg}");
}

/// Translate an outcome + `--exit-code` flag into a typed exit code
/// per Q10's table.
fn apply_exit_code(exit_code_flag: bool, outcome: DiffOutcome) -> DiffExit {
    let code = match (exit_code_flag, outcome) {
        (_, DiffOutcome::Unsupported) => 2_i32,
        (true, DiffOutcome::DiffPresent | DiffOutcome::RemoteAbsent) => 1_i32,
        // false + any, or true + NoChanges → no signal.
        _ => 0_i32,
    };
    DiffExit { code }
}

/// Typed diff flow — reads the local `<name>.toml`, builds the local
/// `BlobEnvelope`, reads back the remote (or local-emulator) entry, and
/// renders the diff via the selected `--format`.
///
/// Returns `Ok(DiffExit { code })` on the success path so the generated
/// CLI's main can call `process::exit(code)` for the non-zero CI gate
/// codes.  Returns `Err(String)` on parse / network / manifest-load
/// errors (the main always exits ≥ 2 for these).
///
/// # Errors
/// Returns a human-readable error string on any load, parse, or
/// envelope verification failure.
#[expect(
    clippy::too_many_lines,
    reason = "config diff orchestration: 6 sequential steps (load + structural checks, envelope, adapter resolve, paths, read, branch) each with its own error handling — extracting sub-functions would just move the lines without reducing conceptual complexity"
)]
#[inline]
pub fn run_config_diff_typed<C>(args: &ConfigDiffArgs) -> Result<DiffExit, String>
where
    C: DeserializeOwned + Serialize + Validate + AppConfigMeta,
{
    // Load + validate (spec 3.3.2: diff runs the same structural
    // checks as push — validate_excluding_secrets + typed_secret_checks +
    // adapter_typed_checks; no consent gate, no re-fetch).
    let validate_args = ConfigValidateArgs {
        app_config: args.app_config.clone(),
        manifest: args.manifest.clone(),
        no_env: args.no_env,
        strict: false,
    };
    let ctx = load_validation_context(&validate_args)?;
    run_shared_checks(&ctx)?;
    let mut opts = AppConfigLoadOptions::default();
    opts.env_overlay = !args.no_env;
    let typed: C = app_config::deserialize_app_config_with_options::<C>(
        &ctx.app_config_path,
        &ctx.app_name,
        &opts,
    )
    .map_err(|err| format_app_config_error(&err))?;
    app_config::validate_excluding_secrets(&typed)
        .map_err(|err| format!("local validation failed: {err}"))?;
    typed_secret_checks(&typed, &ctx)?;
    run_adapter_typed_checks::<C>(&ctx)?;

    // Build the local envelope.
    let local_data: serde_json::Value = serde_json::to_value(&typed)
        .map_err(|err| format!("failed to serialise local config: {err}"))?;
    let local_envelope = BlobEnvelope::new(local_data, generated_at_rfc3339());
    let local_sha = local_envelope.sha256.clone();

    // Resolve adapter + store + key (mirrors the push flow).
    ensure_adapter_defined(&args.adapter, Some(&ctx.manifest_loader))?;
    let adapter = adapter_registry::get_adapter(&args.adapter).ok_or_else(|| {
        format!(
            "adapter `{}` is declared in {} but not registered in this build",
            args.adapter,
            args.manifest.display()
        )
    })?;
    let logical = resolve_config_store_id(args.store.as_deref(), ctx.manifest())?;
    let env_config = EnvConfig::from_env();
    let platform = env_config.store_name("config", &logical);
    let store = ResolvedStoreId::new(logical.clone(), platform);
    let key = args.key.clone().unwrap_or(logical);

    // Resolve adapter paths for the read call.
    let manifest_root = ctx
        .manifest_path
        .parent()
        .filter(|pp| !pp.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let manifest_data = ctx.manifest();
    let (adapter_manifest_path, component_selector) =
        if let Some((_canonical, adapter_cfg)) = manifest_data.adapter_entry(&args.adapter) {
            (
                adapter_cfg.adapter.manifest.clone(),
                adapter_cfg.adapter.component.clone(),
            )
        } else {
            (None, None)
        };
    let mut push_ctx = adapter_registry::AdapterPushContext::new().with_local(args.local);
    if let Some(path) = args.runtime_config.as_deref() {
        push_ctx = push_ctx.with_runtime_config_path(path);
    }
    let paths = PushPathRefs {
        manifest_root,
        adapter_manifest_path: adapter_manifest_path.as_deref(),
        component_selector: component_selector.as_deref(),
        push_ctx: &push_ctx,
    };

    // Read the remote entry.
    let remote = read_remote(adapter, args.local, &paths, &store, &key)?;

    // Branch per variant, render, determine outcome.
    let outcome: DiffOutcome = match &remote {
        ReadConfigEntry::Present(body) => {
            let remote_envelope: BlobEnvelope = serde_json::from_str(body)
                .map_err(|err| format!("remote envelope parse failed: {err}"))?;
            remote_envelope
                .verify()
                .map_err(|err| format!("remote envelope verification failed: {err}"))?;
            if remote_envelope.sha256 == local_sha {
                diff_info(&format!("# no changes (sha256 matches: {local_sha})"));
                DiffOutcome::NoChanges
            } else {
                dispatch_diff_format(
                    &remote_envelope.data,
                    &local_envelope.data,
                    &remote_envelope.sha256,
                    &local_sha,
                    &args.format,
                );
                DiffOutcome::DiffPresent
            }
        }
        ReadConfigEntry::MissingKey => {
            let leaf_count = collect_changes(
                &serde_json::Value::Object(serde_json::Map::default()),
                &local_envelope.data,
            )
            .len();
            diff_info(&format!(
                "# no remote at key `{key}`; all {leaf_count} leaves added"
            ));
            dispatch_diff_format(
                &serde_json::Value::Object(serde_json::Map::default()),
                &local_envelope.data,
                "(none)",
                &local_sha,
                &args.format,
            );
            DiffOutcome::RemoteAbsent
        }
        ReadConfigEntry::MissingStore => {
            let leaf_count = collect_changes(
                &serde_json::Value::Object(serde_json::Map::default()),
                &local_envelope.data,
            )
            .len();
            diff_info(&format!(
                "# store has no matching backend yet \u{2014} run `edgezero provision \
                 --adapter {}` first if this is the live remote; all {leaf_count} leaves added",
                args.adapter
            ));
            dispatch_diff_format(
                &serde_json::Value::Object(serde_json::Map::default()),
                &local_envelope.data,
                "(none)",
                &local_sha,
                &args.format,
            );
            DiffOutcome::RemoteAbsent
        }
        ReadConfigEntry::Unsupported(reason) => {
            diff_info(&format!(
                "config diff for {} is unsupported ({reason}). Re-run with --local for the \
                 on-disk read, or push unconditionally with `<app-cli> config push \
                 --adapter {} --yes` to update without seeing the diff.",
                args.adapter, args.adapter,
            ));
            DiffOutcome::Unsupported
        }
        // `ReadConfigEntry` is `#[non_exhaustive]`; forward-compat fallback.
        _ => DiffOutcome::Unsupported,
    };

    Ok(apply_exit_code(args.exit_code, outcome))
}

/// Dispatch the diff to the correct renderer based on `format`.
///
/// `unified` re-uses `print_unified_diff_inline` from this module (no duplication).
/// `structured` and `json` delegate to `crate::diff`.
fn dispatch_diff_format(
    remote_data: &serde_json::Value,
    local_data: &serde_json::Value,
    remote_sha: &str,
    local_sha: &str,
    format: &DiffFormat,
) {
    match format {
        DiffFormat::Unified => {
            print_unified_diff_inline(remote_data, local_data, remote_sha, local_sha);
        }
        DiffFormat::Structured => {
            render_structured(remote_data, local_data, remote_sha, local_sha);
        }
        DiffFormat::Json => {
            render_json(remote_data, local_data, remote_sha, local_sha);
        }
    }
}

// -------------------------------------------------------------------
// Helpers for run_config_push_typed
// -------------------------------------------------------------------

/// Consent gate: 8.3 Spin Cloud four-branch UX when `remote` is
/// `Unsupported`; 8.2 default flow otherwise.
fn handle_consent(args: &ConfigPushArgs, remote: &ReadConfigEntry) -> Result<(), String> {
    if let ReadConfigEntry::Unsupported(reason) = remote {
        if args.dry_run {
            return Err(format!(
                "config push --dry-run --adapter spin against Spin Cloud is unsupported \
                 ({reason}); re-run with --local for the on-disk SQLite write or drop \
                 --dry-run to write unconditionally with --yes"
            ));
        }
        if !args.yes {
            if !stdin().is_terminal() {
                return Err(format!(
                    "Spin Cloud read-back unsupported ({reason}); pass --yes for \
                     non-interactive runs (the push writes unconditionally)"
                ));
            }
            #[expect(
                clippy::print_stderr,
                reason = "stream discipline: TTY consent prompt goes to stderr; eprint! (no newline) keeps the cursor on the prompt line"
            )]
            {
                eprint!("cannot read remote on Spin Cloud ({reason}); write anyway? [y/N] ");
            };
            let mut buf = String::new();
            stdin()
                .read_line(&mut buf)
                .map_err(|err| format!("stdin read failed: {err}"))?;
            if !matches!(buf.trim(), "y" | "Y") {
                return Err("aborted by operator".into());
            }
        }
        Ok(())
    } else {
        require_consent(args, remote)
    }
}

/// Write an informational message to stderr. All push messages that are
/// not errors go here.
#[expect(
    clippy::print_stderr,
    reason = "stream discipline: informational messages go to stderr, never stdout"
)]
fn push_info(msg: &str) {
    eprintln!("{msg}");
}

/// Dispatch a single read to either `read_config_entry_local` or
/// `read_config_entry` based on `local`. Collapses the repeated
/// if/else adapter-read pattern that appears 3× in the typed push flow.
fn read_remote(
    adapter: &dyn adapter_registry::Adapter,
    local: bool,
    paths: &PushPathRefs<'_>,
    store: &ResolvedStoreId,
    key: &str,
) -> Result<ReadConfigEntry, String> {
    if local {
        adapter.read_config_entry_local(
            paths.manifest_root,
            paths.adapter_manifest_path,
            paths.component_selector,
            store,
            key,
            paths.push_ctx,
        )
    } else {
        adapter.read_config_entry(
            paths.manifest_root,
            paths.adapter_manifest_path,
            paths.component_selector,
            store,
            key,
            paths.push_ctx,
        )
    }
}

/// Re-fetch right before the write to detect concurrent pushes
/// and skip-on-equal.
#[expect(
    clippy::too_many_arguments,
    reason = "recheck needs adapter, local flag, all path refs, store, key, local_sha, first_read, and approved_remote_sha — each is distinct; a sub-struct would shift complexity without simplifying the call site"
)]
fn recheck_before_write(
    adapter: &dyn adapter_registry::Adapter,
    args: &ConfigPushArgs,
    paths: &PushPathRefs<'_>,
    store: &ResolvedStoreId,
    key: &str,
    local_sha: &str,
    first_read: &ReadConfigEntry,
    approved_remote_sha: Option<&str>,
) -> Result<RecheckOutcome, String> {
    let remote_now = read_remote(adapter, args.local, paths, store, key)?;
    if let ReadConfigEntry::Present(body_now) = remote_now {
        let remote_now_env: BlobEnvelope = serde_json::from_str(&body_now)
            .map_err(|err| format!("post-consent remote envelope parse failed: {err}"))?;
        remote_now_env
            .verify()
            .map_err(|err| format!("post-consent remote envelope verification failed: {err}"))?;
        if remote_now_env.sha256 == local_sha {
            push_info(&format!(
                "# concurrent push reached the same state (sha256 matches: {local_sha}); skipping write"
            ));
            return Ok(RecheckOutcome::Skip);
        }
        // Compare against approved_remote_sha (the inner remote_envelope
        // from the first read is out of scope here).
        // approved_remote_sha is None for MissingKey/MissingStore
        // first reads — display "(none)".
        let approved_display = approved_remote_sha.map_or("(none)", short_ref);
        if approved_remote_sha != Some(remote_now_env.sha256.as_str()) {
            push_info(&format!(
                "# warning: remote changed between diff and write (was {} now {}); applying local anyway",
                approved_display,
                short_ref(&remote_now_env.sha256),
            ));
        }
    } else if matches!(first_read, ReadConfigEntry::Present(_)) {
        // First read Present, second read MissingKey/MissingStore:
        // the remote was removed while we waited for consent. Warn
        // and fall through — creating rather than overwriting is fine.
        if let Some(approved) = approved_remote_sha {
            push_info(&format!(
                "# warning: remote was removed between diff and write (was {}); creating fresh",
                short_ref(approved),
            ));
        }
    } else {
        // First read MissingKey/MissingStore, second also Missing —
        // nothing changed; fall through to write silently.
    }
    Ok(RecheckOutcome::Write)
}

/// Render the first-read diff and return the outcome.
///
/// - `Present` with matching SHA → `NoChange`.
/// - `Present` with differing SHA → renders diff (when `!no_diff`) and
///   returns `ProceedFromPresent` with the remote SHA.
/// - `MissingKey` / `MissingStore` → renders "all leaves added" diff
///   (when `!no_diff`) and returns `ProceedFromMissingOrUnsupported`.
/// - `Unsupported` → returns `ProceedFromMissingOrUnsupported` without
///   rendering.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "ReadConfigEntry is #[non_exhaustive]; wildcard covers future variants and the no_diff guard cases"
)]
fn render_first_read_diff(
    remote: &ReadConfigEntry,
    key: &str,
    local_envelope: &BlobEnvelope,
    local_sha: &str,
    no_diff: bool,
) -> Result<FirstReadOutcome, String> {
    match remote {
        ReadConfigEntry::Present(body_str) => {
            let remote_envelope: BlobEnvelope = serde_json::from_str(body_str)
                .map_err(|err| format!("remote envelope parse failed: {err}"))?;
            remote_envelope
                .verify()
                .map_err(|err| format!("remote envelope verification failed: {err}"))?;
            if remote_envelope.sha256 == local_sha {
                return Ok(FirstReadOutcome::NoChange);
            }
            if !no_diff {
                print_unified_diff_inline(
                    &remote_envelope.data,
                    &local_envelope.data,
                    &remote_envelope.sha256,
                    local_sha,
                );
            }
            Ok(FirstReadOutcome::ProceedFromPresent {
                approved_remote_sha: remote_envelope.sha256.clone(),
            })
        }
        ReadConfigEntry::MissingKey if !no_diff => {
            push_info(&format!("# no remote at key `{key}`; all leaves added"));
            print_unified_diff_inline(
                &serde_json::Value::Object(serde_json::Map::default()),
                &local_envelope.data,
                "(none)",
                local_sha,
            );
            Ok(FirstReadOutcome::ProceedFromMissingOrUnsupported)
        }
        ReadConfigEntry::MissingStore if !no_diff => {
            push_info(
                "# store has no matching backend yet \u{2014} run `edgezero provision \
                 --adapter <name>` first if this is the live remote",
            );
            push_info("# no remote store; all leaves added");
            print_unified_diff_inline(
                &serde_json::Value::Object(serde_json::Map::default()),
                &local_envelope.data,
                "(none)",
                local_sha,
            );
            Ok(FirstReadOutcome::ProceedFromMissingOrUnsupported)
        }
        // Unsupported, MissingKey/MissingStore with no_diff, and future
        // #[non_exhaustive] variants — fall through.
        _ => Ok(FirstReadOutcome::ProceedFromMissingOrUnsupported),
    }
}

/// Consent gate for 8.2 default flow (non-Spin-Cloud adapters and all
/// read-capable variants). `--yes` or `--dry-run` bypass the prompt.
/// TTY: prompt. Non-TTY without `--yes`: error.
fn require_consent(args: &ConfigPushArgs, _read: &ReadConfigEntry) -> Result<(), String> {
    if args.yes {
        return Ok(());
    }
    if args.dry_run {
        return Ok(());
    }
    if stdin().is_terminal() {
        #[expect(
            clippy::print_stderr,
            reason = "stream discipline: TTY consent prompt goes to stderr"
        )]
        {
            eprint!("Apply changes? [y/N] ");
        };
        let mut buf = String::new();
        stdin()
            .read_line(&mut buf)
            .map_err(|err| format!("stdin read failed: {err}"))?;
        if !matches!(buf.trim(), "y" | "Y") {
            return Err("aborted by operator".into());
        }
        Ok(())
    } else {
        Err("non-interactive run requires --yes (no TTY available for prompt)".into())
    }
}

/// Resolve the adapter-manifest root, adapter manifest path, component
/// selector, and `AdapterPushContext` from the push context.
///
/// Returns `(manifest_root, adapter_manifest_path, component_selector, push_ctx)`.
#[expect(
    clippy::type_complexity,
    reason = "four-tuple return avoids a dedicated struct for a single call site; the items are immediately destructured by the caller"
)]
fn resolve_push_paths(
    ctx: &PushContext,
) -> Result<
    (
        &Path,
        Option<String>,
        Option<String>,
        adapter_registry::AdapterPushContext<'_>,
    ),
    String,
> {
    let manifest = ctx.validation.manifest();
    let (_canonical, adapter_cfg) =
        manifest.adapter_entry(ctx.adapter.name()).ok_or_else(|| {
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
    let resolved = &ctx.adapter_push_ctx;
    let mut push_ctx = adapter_registry::AdapterPushContext::new().with_local(resolved.local);
    if let Some(path) = resolved.runtime_config_path.as_deref() {
        push_ctx = push_ctx.with_runtime_config_path(path);
    }
    if let Some(deploy_cmd) = adapter_cfg.commands.deploy.as_deref() {
        push_ctx = push_ctx.with_manifest_adapter_deploy_cmd(deploy_cmd);
    }
    let adapter_manifest_path = adapter_cfg.adapter.manifest.clone();
    let component_selector = adapter_cfg.adapter.component.clone();
    Ok((
        manifest_root,
        adapter_manifest_path,
        component_selector,
        push_ctx,
    ))
}

/// Dry-run log + adapter write dispatch.
fn write_envelope(
    adapter: &dyn adapter_registry::Adapter,
    args: &ConfigPushArgs,
    ctx: &PushContext,
    paths: &PushPathRefs<'_>,
    key: &str,
    body: String,
) -> Result<(), String> {
    if args.dry_run {
        log::info!(
            "[edgezero] config push --dry-run{} for `{}` -> store `{}` (platform name `{}`):",
            if args.local { " --local" } else { "" },
            adapter.name(),
            ctx.store.logical,
            ctx.store.platform
        );
    }
    let entries: Vec<(String, String)> = vec![(key.to_owned(), body)];
    let lines = if args.local {
        adapter.push_config_entries_local(
            paths.manifest_root,
            paths.adapter_manifest_path,
            paths.component_selector,
            &ctx.store,
            &entries,
            paths.push_ctx,
            args.dry_run,
        )?
    } else {
        adapter.push_config_entries(
            paths.manifest_root,
            paths.adapter_manifest_path,
            paths.component_selector,
            &ctx.store,
            &entries,
            paths.push_ctx,
            args.dry_run,
        )?
    };
    for line in lines {
        log::info!("{line}");
    }
    Ok(())
}

/// Truncate a SHA to 8 characters for display. Returns the input
/// unchanged when ≤ 8 bytes (the `"(none)"` sentinel is 6 bytes —
/// avoids a panic on the `&sha[..8]` index).
pub(crate) fn short_ref(sha: &str) -> &str {
    if sha.len() <= 8 {
        sha
    } else {
        #[expect(
            clippy::string_slice,
            reason = "checked: sha.len() > 8 ensures this 8-byte index is within ASCII hex bytes"
        )]
        {
            &sha[..8]
        }
    }
}

/// Pretty-print a `serde_json::Value` with object keys sorted
/// recursively by UTF-8 byte order. Used by the diff renderer so two
/// trees with identical content but different source-key order produce
/// identical normalised text (and thus an empty diff).
pub(crate) fn render_for_diff(value: &serde_json::Value) -> String {
    let sorted = sort_keys_recursive(value);
    serde_json::to_string_pretty(&sorted).unwrap_or_else(|_| "<unrenderable>".into())
}

fn sort_keys_recursive(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value as JValue};
    match value {
        JValue::Object(map) => {
            let mut sorted_map = Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|ka, kb| ka.as_bytes().cmp(kb.as_bytes()));
            for key in keys {
                sorted_map.insert(key.clone(), sort_keys_recursive(&map[key]));
            }
            JValue::Object(sorted_map)
        }
        JValue::Array(items) => JValue::Array(items.iter().map(sort_keys_recursive).collect()),
        // Scalar leaf types — clone unchanged. Explicit arms required by
        // `clippy::wildcard_enum_match_arm` (serde_json::Value is #[non_exhaustive]).
        other @ (JValue::Null | JValue::Bool(_) | JValue::Number(_) | JValue::String(_)) => {
            other.clone()
        }
    }
}

/// Write a unified diff of `remote_data` vs `local_data` to `out`.
/// Both trees are normalised via [`render_for_diff`] before diffing so
/// key-ordering differences produce an empty diff. Uses
/// `similar::TextDiff` to produce standard git-style unified diff output.
///
/// **Stream discipline:** diff CONTENT goes to `out` (stdout
/// in production) so operators can pipe through `git diff` colour
/// wrappers. Informational messages go to stderr via `eprintln!` in the
/// caller. Never `log::*` — prefixes corrupt TTY renders and `jq` consumers.
pub(crate) fn print_unified_diff_to_writer<W: Write>(
    remote_data: &serde_json::Value,
    local_data: &serde_json::Value,
    remote_sha: &str,
    local_sha: &str,
    out: &mut W,
) -> Result<(), IoError> {
    let remote_text = render_for_diff(remote_data);
    let local_text = render_for_diff(local_data);
    let diff = TextDiff::from_lines(&remote_text, &local_text);
    let remote_header = format!("remote (sha256: {})", short_ref(remote_sha));
    let local_header = format!("local  (sha256: {})", short_ref(local_sha));
    let mut builder = diff.unified_diff();
    let unified = builder
        .header(&remote_header, &local_header)
        .context_radius(3);
    write!(out, "{unified}")
}

/// Inline unified diff renderer — writes to stdout. Production path
/// for `config push`'s inline-diff prompt. Tests use
/// [`print_unified_diff_to_writer`] to capture into a `Vec<u8>`.
pub(crate) fn print_unified_diff_inline(
    remote_data: &serde_json::Value,
    local_data: &serde_json::Value,
    remote_sha: &str,
    local_sha: &str,
) {
    use std::io::stdout;
    let mut stdout = stdout().lock();
    // Silently ignore write errors — stdout may be a closed pipe (e.g.
    // `edgezero config push | head`). The operator already saw the diff
    // header on stderr; a broken pipe is not a push failure.
    drop(print_unified_diff_to_writer(
        remote_data,
        local_data,
        remote_sha,
        local_sha,
        &mut stdout,
    ));
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
// Blob envelope build — typed push only
// -------------------------------------------------------------------

/// Serialise `typed` to a [`BlobEnvelope`] JSON string.
///
/// Every field — including `#[secret]` and `#[secret(store_ref)]` fields —
/// is stored VERBATIM in the blob. The value at rest for a secret field is
/// the operator-supplied key NAME (e.g. `"demo_api_token"`), NOT the
/// resolved secret value. The runtime extractor
/// (`crates/edgezero-core/src/extractor.rs`, `secret_walk`) reads those
/// key names from `data` and swaps each one for the resolved secret value
/// from the appropriate secret store. Spec 3.3 (secret-key NAMES at rest).
///
/// `generated_at` is stamped with the current UTC second.
///
/// # Errors
/// Returns a human-readable error string if serialisation fails.
fn build_config_envelope<C>(typed: &C) -> Result<String, String>
where
    C: Serialize,
{
    // The blob carries every typed field VERBATIM — including #[secret]
    // and #[secret(store_ref)] fields, whose value at rest is the
    // operator-supplied key NAME (e.g. "demo_api_token"). The runtime
    // extractor reads those key names from data and swaps each one for
    // the resolved secret value. Spec 3.3 (secret-key NAMES at rest).
    let data: serde_json::Value = serde_json::to_value(typed)
        .map_err(|err| format!("failed to serialise typed config: {err}"))?;
    let envelope = BlobEnvelope::new(data, generated_at_rfc3339());
    serde_json::to_string(&envelope).map_err(|err| format!("failed to serialise envelope: {err}"))
}

/// Current UTC timestamp formatted as RFC 3339 with second precision and
/// a trailing `Z` (`2026-06-17T18:42:31Z`). Matches spec 4.1 example.
/// `generated_at` is informational only — it is NOT part of the SHA.
fn generated_at_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
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

/// Collect every concrete secret leaf a `SecretField` resolves to in the
/// raw app-config TOML, navigating `Field` (table descent) and `ArrayEach`
/// (per-element) segments. `label` uses concrete `[n]` indices and, for a
/// `KeyInNamedStore` leaf, `store_ref_value` is resolved from the leaf's
/// innermost parent table. Absent optional leaves yield nothing; a missing
/// required leaf yields an `Err` carrying the dotted label.
fn collect_secret_leaves<'raw>(
    root: &'raw Value,
    field: &SecretField,
) -> Result<Vec<ResolvedTomlLeaf<'raw>>, String> {
    fn walk<'raw>(
        node: &'raw Value,
        field: &SecretField,
        remaining: &[SecretPathSegment],
        rendered: &str,
        out: &mut Vec<ResolvedTomlLeaf<'raw>>,
    ) -> Result<(), String> {
        match remaining.split_first() {
            Some((SecretPathSegment::Field(name), [])) => {
                let parent = node.as_table().ok_or_else(|| {
                    format!("expected a table containing `{name}` at `{rendered}`")
                })?;
                let leaf_label = if rendered.is_empty() {
                    name.to_string()
                } else {
                    format!("{rendered}.{name}")
                };
                match parent.get(name.as_ref()).and_then(Value::as_str) {
                    Some(value) => {
                        let store_ref_value = match field.kind {
                            SecretKind::KeyInNamedStore { store_ref_field } => {
                                parent.get(store_ref_field).and_then(Value::as_str)
                            }
                            SecretKind::KeyInDefault | SecretKind::StoreRef => None,
                        };
                        out.push(ResolvedTomlLeaf {
                            label: leaf_label,
                            store_ref_value,
                            value,
                        });
                        Ok(())
                    }
                    None if field.optional && parent.get(name.as_ref()).is_none() => Ok(()),
                    None => Err(format!(
                        "`#[secret]` field `{leaf_label}` is missing or not a string"
                    )),
                }
            }
            Some((SecretPathSegment::Field(name), rest)) => {
                let table = node
                    .as_table()
                    .ok_or_else(|| format!("expected a table at `{rendered}`"))?;
                let next_rendered = if rendered.is_empty() {
                    name.to_string()
                } else {
                    format!("{rendered}.{name}")
                };
                // Intermediates are always required — `field.optional` reflects
                // only the leaf, and the derive never nests through `Option`. This
                // matches the runtime walk (`resolve_secret_field`) so `config
                // validate` catches exactly what the runtime would reject.
                match table.get(name.as_ref()) {
                    Some(child) => walk(child, field, rest, &next_rendered, out),
                    None => Err(format!("missing `{next_rendered}`")),
                }
            }
            Some((SecretPathSegment::ArrayEach, rest)) => {
                let arr = node
                    .as_array()
                    .ok_or_else(|| format!("expected an array at `{rendered}`"))?;
                for (idx, item) in arr.iter().enumerate() {
                    let indexed = format!("{rendered}[{idx}]");
                    walk(item, field, rest, &indexed, out)?;
                }
                Ok(())
            }
            None => Ok(()),
        }
    }
    let mut out = Vec::new();
    walk(root, field, &field.path, "", &mut out)?;
    Ok(out)
}

/// Typed-only adapter dispatch: feed each adapter the `#[secret]`
/// (`KeyInDefault` and `KeyInNamedStore` — `StoreRef` values are
/// runtime store ids, not flat-namespace candidates) so adapters
/// whose secret store has a flat-namespace constraint (Spin) can
/// detect within-secrets collisions.
fn run_adapter_typed_checks<C: AppConfigMeta>(ctx: &ValidationContext) -> Result<(), String> {
    let default_store_id = ctx
        .manifest()
        .stores
        .secrets
        .as_ref()
        .map(StoreDeclaration::default_id);
    let mut entries: Vec<TypedSecretEntry<'_>> = Vec::new();
    for field in C::secret_fields() {
        for leaf in collect_secret_leaves(&ctx.raw_config, &field)? {
            match field.kind {
                SecretKind::KeyInDefault => {
                    if let Some(store_id) = default_store_id {
                        entries.push(TypedSecretEntry::new(store_id, leaf.label, leaf.value));
                    }
                }
                SecretKind::KeyInNamedStore { .. } => {
                    let store_id = leaf.store_ref_value.ok_or_else(|| {
                        format!(
                            "`#[secret(store_ref = \"...\")]` field `{}` is missing its store_ref sibling",
                            leaf.label
                        )
                    })?;
                    entries.push(TypedSecretEntry::new(store_id, leaf.label, leaf.value));
                }
                SecretKind::StoreRef => {}
            }
        }
    }

    for name in ctx.manifest().adapters.keys() {
        if let Some(adapter) = adapter_registry::get_adapter(name) {
            adapter.validate_typed_secrets(&entries)?;
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
    for field in C::secret_fields() {
        for leaf in collect_secret_leaves(&ctx.raw_config, &field)? {
            let label = leaf.label;
            let value = leaf.value;
            if value.is_empty() {
                return Err(format!(
                    "{}: `#[secret]` field `{}` must be non-empty",
                    ctx.app_config_path.display(),
                    label
                ));
            }
            match field.kind {
                SecretKind::KeyInDefault => {
                    if ctx.manifest().stores.secrets.is_none() {
                        return Err(format!(
                            "{}: `#[secret]` field `{}` requires `[stores.secrets]` to be declared in {}",
                            ctx.app_config_path.display(),
                            label,
                            ctx.manifest_path.display()
                        ));
                    }
                }
                SecretKind::KeyInNamedStore { .. } => {
                    // The field value is a key within a named store; the named
                    // store is identified by the sibling `#[secret(store_ref)]`
                    // field. Verify the store section is at least declared.
                    if ctx.manifest().stores.secrets.is_none() {
                        return Err(format!(
                            "{}: `#[secret(store_ref = \"...\")]` field `{}` requires `[stores.secrets]` to be declared in {}",
                            ctx.app_config_path.display(),
                            label,
                            ctx.manifest_path.display()
                        ));
                    }
                }
                SecretKind::StoreRef => {
                    let secrets = ctx.manifest().stores.secrets.as_ref().ok_or_else(|| {
                        format!(
                            "{}: `#[secret(store_ref)]` field `{}` requires `[stores.secrets]` to be declared in {}",
                            ctx.app_config_path.display(),
                            label,
                            ctx.manifest_path.display()
                        )
                    })?;
                    if !secrets.ids.iter().any(|id| id == value) {
                        return Err(format!(
                            "{}: `#[secret(store_ref)]` field `{}` = {:?} is not in [stores.secrets].ids ({:?})",
                            ctx.app_config_path.display(),
                            label,
                            value,
                            secrets.ids
                        ));
                    }
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
    clippy::arbitrary_source_item_ordering,
    clippy::default_numeric_fallback,
    reason = "test module groups items by subject (PathPrepend co-located with its fake-spin consumers, fixtures with their tests) rather than by item kind; `range(min = 100, max = 60_000)` bounds default to the field's int type"
)]
mod tests {
    use super::*;
    use crate::test_support::{manifest_guard, EnvOverride};
    use serde::{Deserialize, Serialize};
    use std::borrow::Cow;
    #[cfg(unix)]
    use std::ffi::OsString;
    use std::fs;
    #[cfg(unix)]
    use std::sync::Mutex;
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
        fn secret_fields() -> Vec<SecretField> {
            vec![
                SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![SecretPathSegment::Field(Cow::Borrowed("api_token"))],
                    optional: false,
                },
                SecretField {
                    kind: SecretKind::StoreRef,
                    path: vec![SecretPathSegment::Field(Cow::Borrowed("vault"))],
                    optional: false,
                },
            ]
        }
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
            key: None,
            local: false,
            manifest: manifest.to_path_buf(),
            no_diff: false,
            no_env: true,
            runtime_config: None,
            store: None,
            yes: false,
        }
    }

    // ---------- raw flow ----------

    #[test]
    fn raw_validates_a_well_formed_project() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let (_dir, manifest, _) = setup_project(VALID_MANIFEST, VALID_APP_CONFIG);
        run_config_validate(&args_for(&manifest)).expect("valid project passes");
    }

    #[test]
    fn raw_errors_on_unknown_manifest_path() {
        let _lock = manifest_guard().lock().expect("manifest guard");
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
        let _lock = manifest_guard().lock().expect("manifest guard");
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

    /// High 2 — spec 3.3.8: typed validate uses `validate_excluding_secrets`,
    /// so a `#[secret]` field annotated with `length(min = 32)` must NOT
    /// reject a short key name like `"short_key"` (9 bytes).  The runtime
    /// resolves it to the real secret value and runs the full validator there.
    #[test]
    fn validate_typed_skips_secret_field_validators() {
        #[derive(Debug, Deserialize, Serialize, Validate)]
        #[serde(deny_unknown_fields)]
        struct SecretValidatorConfig {
            #[validate(length(min = 32_u64))]
            api_token: String,
            #[validate(length(min = 1_u64))]
            greeting: String,
        }
        impl AppConfigMeta for SecretValidatorConfig {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![SecretPathSegment::Field(Cow::Borrowed("api_token"))],
                    optional: false,
                }]
            }
        }

        let app_config = r#"
api_token = "short_key"
greeting = "hello"
"#;
        let manifest = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.secrets]
ids = ["default"]
"#;
        let (_dir, manifest_path, _) = setup_project(manifest, app_config);
        // "short_key" is 9 chars — would fail length(min=32) on the old full-validate
        // path but MUST PASS with validate_excluding_secrets.
        run_config_validate_typed::<SecretValidatorConfig>(&args_for(&manifest_path))
            .expect("secret-field validator must be skipped on typed validate");
    }

    // ---------- Task 6: path-aware nested / array secret reflection ----------

    // Real nested derive: integrations.datadome.server_side_key (KeyInDefault),
    // partners[*].api_key (KeyInDefault).
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct DataDome {
        #[secret]
        server_side_key: String,
    }
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Integrations {
        #[app_config(nested)]
        #[validate(nested)]
        datadome: DataDome,
    }
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Partner {
        #[secret]
        api_key: String,
    }
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct NestedCliConfig {
        #[app_config(nested)]
        #[validate(nested)]
        integrations: Integrations,
        #[app_config(nested)]
        #[validate(nested)]
        partners: Vec<Partner>,
    }

    const NESTED_MANIFEST: &str = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;

    #[test]
    fn validate_typed_accepts_well_formed_nested_and_array_secrets() {
        let app_config = r#"
[integrations.datadome]
server_side_key = "dd_key"

[[partners]]
api_key = "p0"

[[partners]]
api_key = "p1"
"#;
        let (_dir, manifest_path, _) = setup_project(NESTED_MANIFEST, app_config);
        run_config_validate_typed::<NestedCliConfig>(&args_for(&manifest_path))
            .expect("well-formed nested + array secret config validates");
    }

    #[test]
    fn validate_typed_reports_dotted_path_for_empty_array_secret() {
        // partners[1].api_key is empty -> typed_secret_checks must reject it and
        // name the INDEXED dotted path.
        let app_config = r#"
[integrations.datadome]
server_side_key = "dd_key"

[[partners]]
api_key = "p0"

[[partners]]
api_key = ""
"#;
        let (_dir, manifest_path, _) = setup_project(NESTED_MANIFEST, app_config);
        let err = run_config_validate_typed::<NestedCliConfig>(&args_for(&manifest_path))
            .expect_err("empty array secret must be rejected");
        assert!(
            err.contains("partners[1].api_key"),
            "error names the indexed dotted path: {err}"
        );
    }

    #[test]
    fn validate_typed_rejects_missing_required_nested_leaf_at_deserialize() {
        // A MISSING required nested leaf fails serde DESERIALIZATION
        // before `typed_secret_checks`/`run_adapter_typed_checks` ever run — so
        // this is deserialize-path coverage, NOT proof of the path-aware
        // collector. The direct collector test below covers that.
        let app_config = r#"
[integrations.datadome]

[[partners]]
api_key = "p0"
"#;
        let (_dir, manifest_path, _) = setup_project(NESTED_MANIFEST, app_config);
        let err = run_config_validate_typed::<NestedCliConfig>(&args_for(&manifest_path))
            .expect_err("missing nested leaf must be rejected");
        assert!(
            err.contains("server_side_key"),
            "error names the missing nested leaf: {err}"
        );
    }

    // Direct coverage of the path-aware TOML collector (the new logic).
    // Bypasses `run_config_validate_typed` so deserialization does not preempt
    // it — proves the collector itself resolves array indices and reports the
    // dotted label for a present-but-invalid / missing leaf.
    #[test]
    fn collect_secret_leaves_resolves_array_indices_and_dotted_labels() {
        let raw: Value = toml::from_str(
            r#"
[[partners]]
api_key = "p0"

[[partners]]
api_key = "p1"
"#,
        )
        .expect("toml");

        let field = SecretField {
            kind: SecretKind::KeyInDefault,
            path: vec![
                SecretPathSegment::Field(Cow::Borrowed("partners")),
                SecretPathSegment::ArrayEach,
                SecretPathSegment::Field(Cow::Borrowed("api_key")),
            ],
            optional: false,
        };
        let leaves = collect_secret_leaves(&raw, &field).expect("collect");
        let labels: Vec<&str> = leaves.iter().map(|leaf| leaf.label.as_str()).collect();
        assert_eq!(labels, vec!["partners[0].api_key", "partners[1].api_key"]);
        let values: Vec<&str> = leaves.iter().map(|leaf| leaf.value).collect();
        assert_eq!(values, vec!["p0", "p1"]);
    }

    #[test]
    fn collect_secret_leaves_errors_on_missing_required_leaf_with_dotted_label() {
        let raw: Value = toml::from_str(
            r#"
[integrations.datadome]
other = "x"
"#,
        )
        .expect("toml");

        let field = SecretField {
            kind: SecretKind::KeyInDefault,
            path: vec![
                SecretPathSegment::Field(Cow::Borrowed("integrations")),
                SecretPathSegment::Field(Cow::Borrowed("datadome")),
                SecretPathSegment::Field(Cow::Borrowed("server_side_key")),
            ],
            optional: false,
        };
        let err = collect_secret_leaves(&raw, &field).expect_err("missing required leaf");
        assert!(
            err.contains("integrations.datadome.server_side_key"),
            "collector error names the dotted path: {err}"
        );
    }

    #[test]
    fn collect_secret_leaves_skips_absent_optional_leaf() {
        // An optional (`Option<String>`) secret leaf that's absent from the TOML
        // yields nothing — no error. The parent table exists; only the leaf key
        // is missing.
        let raw: Value = toml::from_str("[integrations.datadome]\nother = \"x\"\n").expect("toml");
        let field = SecretField {
            kind: SecretKind::KeyInDefault,
            path: vec![
                SecretPathSegment::Field(Cow::Borrowed("integrations")),
                SecretPathSegment::Field(Cow::Borrowed("datadome")),
                SecretPathSegment::Field(Cow::Borrowed("webhook_key")),
            ],
            optional: true,
        };
        let leaves = collect_secret_leaves(&raw, &field).expect("absent optional leaf is ok");
        assert!(leaves.is_empty(), "absent optional leaf yields nothing");
    }

    #[test]
    fn collect_secret_leaves_errors_on_missing_required_intermediate() {
        // A missing INTERMEDIATE (the `integrations` table) is an error even
        // when the leaf is optional — `optional` reflects only the leaf, and
        // intermediates are structurally required. Locks alignment with the
        // runtime walk (`resolve_secret_field`).
        let raw: Value = toml::from_str("other = \"x\"\n").expect("toml");
        let field = SecretField {
            kind: SecretKind::KeyInDefault,
            path: vec![
                SecretPathSegment::Field(Cow::Borrowed("integrations")),
                SecretPathSegment::Field(Cow::Borrowed("datadome")),
                SecretPathSegment::Field(Cow::Borrowed("webhook_key")),
            ],
            optional: true,
        };
        let err = collect_secret_leaves(&raw, &field).expect_err("missing required intermediate");
        assert!(
            err.contains("integrations"),
            "collector error names the missing intermediate: {err}"
        );
    }

    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Vaulted {
        #[secret(store_ref = "vault")]
        token: String,
        #[secret(store_ref)]
        vault: String,
    }
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct NamedStoreCliConfig {
        #[app_config(nested)]
        #[validate(nested)]
        vaulted: Vaulted,
    }

    #[test]
    fn validate_typed_accepts_nested_named_store_with_sibling() {
        let manifest = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default", "named"]
default = "default"
"#;
        let app_config = r#"
[vaulted]
token = "tok_key"
vault = "named"
"#;
        let (_dir, manifest_path, _) = setup_project(manifest, app_config);
        run_config_validate_typed::<NamedStoreCliConfig>(&args_for(&manifest_path))
            .expect("nested named-store secret with a declared store validates");
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
        let _lock = manifest_guard().lock().expect("manifest guard");
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
        let _lock = manifest_guard().lock().expect("manifest guard");
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
        let _lock = manifest_guard().lock().expect("manifest guard");
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
        let _lock = manifest_guard().lock().expect("manifest guard");
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
        let _lock = manifest_guard().lock().expect("manifest guard");
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
        let _lock = manifest_guard().lock().expect("manifest guard");
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
        // walker treated every secret_fields() entry as a potential
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
            fn secret_fields() -> Vec<SecretField> {
                vec![
                    SecretField {
                        kind: SecretKind::KeyInDefault,
                        path: vec![SecretPathSegment::Field(Cow::Borrowed("api_token"))],
                        optional: false,
                    },
                    SecretField {
                        kind: SecretKind::StoreRef,
                        path: vec![SecretPathSegment::Field(Cow::Borrowed("vault"))],
                        optional: false,
                    },
                ]
            }
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
        let _lock = manifest_guard().lock().expect("manifest guard");
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
        let _lock = manifest_guard().lock().expect("manifest guard");
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

    // ---------- raw push (stub-pointer, spec 3.2.1) ----------

    /// Spec 3.2.1: `config push` on the bundled `edgezero` binary is a
    /// stub-pointer. The blob app-config rewrite requires a typed downstream
    /// CLI; the bundled binary has no `AppConfig<C>` in scope. The subcommand
    /// must always return `Err(...)` with a pointer to the typed downstream CLI.
    #[test]
    fn run_config_push_is_stub_pointer_in_bundled_binary() {
        let (_dir, manifest, _) = setup_project(PUSH_MANIFEST, VALID_APP_CONFIG);
        let err = run_config_push(&push_args(&manifest, "axum")).expect_err(
            "bundled run_config_push must always error (spec \u{a7}3.2.1 stub-pointer)",
        );
        assert!(
            err.contains("not supported") && err.contains("typed downstream CLI"),
            "error must be the \u{a7}3.2.1 stub-pointer message: {err}"
        );
        assert!(
            err.contains("\u{a7}3.2.1") || err.contains("3.2.1"),
            "error must reference the spec section: {err}"
        );
    }

    // ---------- typed push ----------

    #[test]
    fn typed_push_writes_blob_envelope_to_local_config_file() {
        use edgezero_core::blob_envelope::{BlobEnvelope, ENVELOPE_VERSION_V1};

        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.app_config = Some(dir.path().join("demo-app.toml"));
        // --yes bypasses the non-TTY consent gate (CI has no TTY and the
        // local store file doesn't exist yet → MissingStore → consent needed).
        args.yes = true;
        run_config_push_typed::<FixtureConfig>(&args).expect("typed push succeeds");

        // Axum writes { "app_config": "<envelope_json>" } — the key is
        // the logical store id and the value is the serialised BlobEnvelope.
        let raw = fs::read_to_string(dir.path().join(".edgezero/local-config-app_config.json"))
            .expect("read written file");
        let outer: serde_json::Value = serde_json::from_str(&raw).expect("valid outer JSON");
        let envelope_str = outer["app_config"]
            .as_str()
            .expect("outer key must be the logical store id");
        let envelope: BlobEnvelope =
            serde_json::from_str(envelope_str).expect("envelope JSON must parse");

        // Envelope integrity
        envelope.verify().expect("envelope SHA must verify");
        assert_eq!(envelope.version, ENVELOPE_VERSION_V1);

        // FixtureConfig: ALL fields — including `api_token` (#[secret]) and
        // `vault` (#[secret(store_ref)]) — must be PRESENT in envelope.data.
        // Their value at rest is the operator-supplied key NAME, not the
        // resolved secret value. The runtime extractor (`secret_walk`) reads
        // those key names and swaps them for the resolved secret. Spec 3.3.
        let data = &envelope.data;
        assert_eq!(
            data.get("api_token").and_then(|val| val.as_str()),
            Some("demo_api_token"),
            "#[secret] field must be preserved in envelope.data as the key name: {data}"
        );
        assert_eq!(
            data.get("vault").and_then(|val| val.as_str()),
            Some("default"),
            "#[secret(store_ref)] field must be preserved in envelope.data: {data}"
        );
        // Non-secret fields also survive.
        assert_eq!(data["greeting"], "hello");
        // Nested struct serialises as a JSON object (not flattened).
        assert_eq!(data["service"]["timeout_ms"], 1500_i32);
    }

    #[test]
    fn typed_push_envelope_sha_matches_hand_computed_hash() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use edgezero_core::canonical_form::canonical_data_sha256;

        // Prove that the SHA embedded in the pushed envelope matches
        // the canonical-form SHA for the same data, computed via the
        // public `canonical_data_sha256` function.  Uses a minimal
        // FixtureConfig with known field values so the expected JSON
        // object is deterministic.

        // All fields are present verbatim — secret key names included.
        // Spec 3.3: secret-field VALUES at rest are the operator-supplied
        // key NAMEs (not the resolved secret values).
        let data = serde_json::json!({
            "api_token": "demo_api_token",
            "greeting": "hello",
            "service": { "timeout_ms": 1500_i32 },
            "vault": "default"
        });
        let expected_sha = canonical_data_sha256(&data);

        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.app_config = Some(dir.path().join("demo-app.toml"));
        // --yes bypasses the non-TTY consent gate in CI.
        args.yes = true;
        run_config_push_typed::<FixtureConfig>(&args).expect("typed push succeeds");

        let raw = fs::read_to_string(dir.path().join(".edgezero/local-config-app_config.json"))
            .expect("read written file");
        let outer: serde_json::Value = serde_json::from_str(&raw).expect("valid outer JSON");
        let envelope_str = outer["app_config"].as_str().expect("outer key");
        let envelope: BlobEnvelope = serde_json::from_str(envelope_str).expect("envelope parses");

        assert_eq!(
            envelope.sha256, expected_sha,
            "envelope SHA must match hand-computed canonical-form hash"
        );
    }

    /// Spec 3.3: the blob MUST carry the secret key NAME verbatim.
    /// The runtime extractor (`secret_walk`) reads that name to look up
    /// the resolved value in the secret store. Stripping the field would
    /// cause `ConfigOutOfDate` on every request after a push.
    #[test]
    fn build_config_envelope_preserves_nested_and_array_secret_names() {
        use edgezero_core::blob_envelope::BlobEnvelope;

        // Push serialises the typed struct verbatim, so nested + array secret
        // KEY NAMES must survive into envelope.data at their full path — the
        // runtime walk reads them there. (`build_config_envelope` only needs
        // `Serialize`.)
        #[derive(Debug, Serialize)]
        struct DataDome {
            server_side_key: String,
        }
        #[derive(Debug, Serialize)]
        struct Integrations {
            datadome: DataDome,
        }
        #[derive(Debug, Serialize)]
        struct Partner {
            api_key: String,
        }
        #[derive(Debug, Serialize)]
        struct NestedPushConfig {
            integrations: Integrations,
            partners: Vec<Partner>,
        }

        let typed = NestedPushConfig {
            integrations: Integrations {
                datadome: DataDome {
                    server_side_key: "dd_key".to_owned(),
                },
            },
            partners: vec![
                Partner {
                    api_key: "p0".to_owned(),
                },
                Partner {
                    api_key: "p1".to_owned(),
                },
            ],
        };

        let json = build_config_envelope(&typed).expect("envelope serialises");
        let envelope: BlobEnvelope = serde_json::from_str(&json).expect("envelope parses");
        assert_eq!(
            envelope.data["integrations"]["datadome"]["server_side_key"].as_str(),
            Some("dd_key"),
            "nested secret key name must survive at its path: {:?}",
            envelope.data
        );
        assert_eq!(
            envelope.data["partners"][0]["api_key"].as_str(),
            Some("p0"),
            "array secret key name (element 0) must survive: {:?}",
            envelope.data
        );
        assert_eq!(
            envelope.data["partners"][1]["api_key"].as_str(),
            Some("p1"),
            "array secret key name (element 1) must survive: {:?}",
            envelope.data
        );
    }

    #[test]
    fn build_config_envelope_preserves_secret_field_values() {
        use edgezero_core::blob_envelope::BlobEnvelope;

        #[derive(Debug, Serialize, Validate)]
        struct SimpleSecret {
            api_token: String,
            greeting: String,
        }

        let typed = SimpleSecret {
            api_token: "demo_api_token".to_owned(),
            greeting: "hello".to_owned(),
        };
        let json = build_config_envelope(&typed).expect("envelope serialises");
        let envelope: BlobEnvelope = serde_json::from_str(&json).expect("envelope parses");
        assert_eq!(
            envelope.data.get("api_token").and_then(|val| val.as_str()),
            Some("demo_api_token"),
            "secret field key name must be preserved in envelope.data: {:?}",
            envelope.data
        );
        assert_eq!(
            envelope.data.get("greeting").and_then(|val| val.as_str()),
            Some("hello"),
            "non-secret field must survive in envelope.data"
        );
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

    /// 5.4: `--key` overrides the default logical store id used as the
    /// blob key. Without `--key`, the written file is keyed by the
    /// manifest's `[stores.config]` id (`"app_config"`). With
    /// `--key staging`, the blob must appear under "staging" instead.
    #[test]
    fn push_typed_honours_key_override() {
        // --- Without --key: key == store.logical ("app_config") ---
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.yes = true;
        run_config_push_typed::<FixtureConfig>(&args).expect("default key push succeeds");
        let default_file = dir.path().join(".edgezero/local-config-app_config.json");
        assert!(
            default_file.exists(),
            "default key write must land in app_config file: {default_file:?}"
        );
        let raw = fs::read_to_string(&default_file).expect("read default key file");
        let outer: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert!(
            outer.get("app_config").is_some(),
            "default key must be app_config: {outer}"
        );

        // --- With --key staging: key == "staging" ---
        let (dir2, manifest2, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args2 = push_args(&manifest2, "axum");
        args2.yes = true;
        args2.key = Some("staging".to_owned());
        run_config_push_typed::<FixtureConfig>(&args2).expect("--key staging push succeeds");
        let staging_file = dir2.path().join(".edgezero/local-config-app_config.json");
        let raw2 = fs::read_to_string(&staging_file).expect("read staging file");
        let outer2: serde_json::Value = serde_json::from_str(&raw2).expect("valid JSON");
        assert!(
            outer2.get("staging").is_some(),
            "--key staging must write under 'staging' key: {outer2}"
        );
        assert!(
            outer2.get("app_config").is_none(),
            "--key staging must NOT write under the default 'app_config' key: {outer2}"
        );
        drop(dir);
        drop(dir2);
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
    fn typed_push_runs_spin_adapter_manifest_check_before_push() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let app_config = r#"
api_token = "x"
greeting = "hi"
vault = "default"

[service]
timeout_ms = 1500
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
        let err = run_config_push_typed::<FixtureConfig>(&push_args(&manifest, "spin"))
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

    // -------------------------------------------------------------------
    // run_config_push_typed — 8.2 consent rules + diff
    // -------------------------------------------------------------------

    /// Build a valid `BlobEnvelope` JSON string for the given data, suitable
    /// for writing into axum's `.edgezero/local-config-<id>.json` as the
    /// "remote" state. The SHA is computed over the canonical-form data
    /// exactly as `build_config_envelope` does — every field (including
    /// `#[secret]` key NAMES per Model A) is preserved verbatim.
    fn make_envelope_json(data: serde_json::Value) -> String {
        use edgezero_core::blob_envelope::BlobEnvelope;
        let env = BlobEnvelope::new(data, "2026-01-01T00:00:00Z".to_owned());
        serde_json::to_string(&env).expect("envelope serialises")
    }

    /// Write a pre-existing "remote" envelope for the axum adapter's
    /// local store file so that `read_config_entry` returns `Present`.
    fn write_remote_envelope(dir: &Path, logical_store_id: &str, envelope_json: &str) {
        let edgezero_dir = dir.join(".edgezero");
        fs::create_dir_all(&edgezero_dir).expect("create .edgezero");
        let map = serde_json::json!({ logical_store_id: envelope_json });
        fs::write(
            edgezero_dir.join(format!("local-config-{logical_store_id}.json")),
            serde_json::to_string(&map).expect("serialize"),
        )
        .expect("write remote envelope file");
    }

    // ---------- deserialise + validate_excluding_secrets ----------

    /// 3.3.8 rule: a `#[secret]` field's VALUE is a key name like
    /// "my-prod-api-key", which may be shorter than a `length(min = 32)`
    /// rule intended for the resolved runtime value. The old
    /// `load_app_config_with_options` path validated the key name against
    /// that rule and rejected the push. The new path uses
    /// `deserialize_app_config_with_options` + `validate_excluding_secrets`
    /// and skips secret-field validators, so the push SUCCEEDS here.
    #[test]
    fn c4_secret_validator_skipped_on_typed_push() {
        // A config type where `api_token` is `#[secret]` AND has a
        // `length(min = 32)` rule. The fixture value "short-key" is 9
        // bytes — it would fail the old path but must pass the new path.
        #[derive(Debug, Deserialize, Serialize, Validate)]
        #[serde(deny_unknown_fields)]
        struct SecretValidatorConfig {
            #[validate(length(min = 32_u64))]
            api_token: String,
            #[validate(length(min = 1_u64))]
            greeting: String,
        }
        impl AppConfigMeta for SecretValidatorConfig {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![SecretPathSegment::Field(Cow::Borrowed("api_token"))],
                    optional: false,
                }]
            }
        }

        let app_config = r#"
api_token = "short-key"
greeting = "hello"
"#;
        let manifest = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        let (_dir, manifest_path, _) = setup_project(manifest, app_config);
        let mut args = push_args(&manifest_path, "axum");
        // --yes bypasses the non-TTY consent error in CI.
        args.yes = true;
        // The push must SUCCEED — secret validators are skipped.
        run_config_push_typed::<SecretValidatorConfig>(&args)
            .expect("secret-field validator must be skipped on typed push");
    }

    // ---------- skip-on-equal (sha match) ----------

    #[test]
    fn c4_skip_on_equal_exits_early_when_sha_matches() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.app_config = Some(dir.path().join("demo-app.toml"));
        args.yes = true;

        // First push to get the canonical envelope on disk.
        run_config_push_typed::<FixtureConfig>(&args).expect("first push succeeds");

        // Second push: same config, so SHA matches. The function must
        // return Ok(()) early (skip-on-equal) without overwriting anything.
        // We verify by checking mtime would be the same — but simpler:
        // just assert it returns Ok(()) with no error.
        run_config_push_typed::<FixtureConfig>(&args)
            .expect("second push with same content must exit Ok via skip-on-equal");
    }

    // ---------- 8.2 consent gate ----------

    #[test]
    fn c4_non_tty_without_yes_errors_on_consent() {
        // CI runs without a TTY. Without --yes, require_consent should
        // return an error explaining the non-interactive constraint.
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.app_config = Some(dir.path().join("demo-app.toml"));
        // Write a DIFFERENT remote envelope so skip-on-equal doesn't fire
        // and we reach the consent gate.
        let other_data = serde_json::json!({ "greeting": "different" });
        let other_env = make_envelope_json(other_data);
        write_remote_envelope(dir.path(), "app_config", &other_env);

        // No --yes flag; non-TTY context (CI); must error at consent.
        let err = run_config_push_typed::<FixtureConfig>(&args)
            .expect_err("non-TTY without --yes must error at consent");
        assert!(
            err.contains("--yes") || err.contains("non-interactive"),
            "error must explain non-interactive constraint: {err}"
        );
    }

    #[test]
    fn c4_yes_flag_bypasses_consent_and_writes() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.app_config = Some(dir.path().join("demo-app.toml"));
        // Write a DIFFERENT remote so skip-on-equal doesn't fire.
        let other_data = serde_json::json!({ "greeting": "old-value" });
        let other_env = make_envelope_json(other_data);
        write_remote_envelope(dir.path(), "app_config", &other_env);

        args.yes = true;
        // With --yes, must proceed past consent and write successfully.
        run_config_push_typed::<FixtureConfig>(&args).expect("--yes must bypass consent and write");

        // Verify the file was updated (the new envelope contains "hello").
        let raw = fs::read_to_string(dir.path().join(".edgezero/local-config-app_config.json"))
            .expect("file written");
        assert!(
            raw.contains("greeting"),
            "written envelope contains greeting field: {raw}"
        );
    }

    #[test]
    fn c4_dry_run_does_not_write_and_bypasses_consent() {
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.app_config = Some(dir.path().join("demo-app.toml"));
        args.dry_run = true;

        // dry_run=true: require_consent passes (dry-run bypass), no write.
        run_config_push_typed::<FixtureConfig>(&args).expect("dry-run typed push must succeed");
        assert!(
            !dir.path().join(".edgezero").exists(),
            ".edgezero must not be created in dry-run"
        );
    }

    #[test]
    fn c4_no_diff_flag_suppresses_diff_render() {
        // With --no-diff + --yes, the push succeeds with no diff rendered
        // (we can't capture stdout here, but the test verifies the
        // function path doesn't error when --no-diff is set).
        let _lock = manifest_guard().lock().expect("manifest guard");
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.app_config = Some(dir.path().join("demo-app.toml"));
        // Pre-write a different remote so skip-on-equal doesn't short-circuit.
        let other_data = serde_json::json!({ "greeting": "old" });
        write_remote_envelope(dir.path(), "app_config", &make_envelope_json(other_data));

        args.no_diff = true;
        args.yes = true;
        run_config_push_typed::<FixtureConfig>(&args)
            .expect("--no-diff --yes typed push must succeed");
    }

    // ---------- 8.3 Spin Cloud Unsupported four-branch UX ----------
    //
    // The Spin adapter returns ReadConfigEntry::Unsupported when the
    // deploy command targets Fermyon Cloud ("spin deploy" / "spin cloud
    // deploy"). We use that to exercise the four-branch UX defined in
    // spec 8.3. The manifest uses `deploy = "spin deploy"` to trigger
    // the Fermyon Cloud detection path inside `read_config_entry`.

    // --- PATH-mutation helpers (mirrors Cloudflare adapter test pattern) ---

    /// RAII guard: prepends `extra` to `$PATH` and restores the original
    /// value on drop. Serialise with [`path_mutation_guard`] so parallel
    /// tests don't race on PATH.
    #[cfg(unix)]
    struct PathPrepend {
        original: Option<OsString>,
    }

    #[cfg(unix)]
    impl PathPrepend {
        fn new(extra: &Path) -> Self {
            use std::env;
            let original = env::var_os("PATH");
            let new_path = match &original {
                Some(prev) => {
                    let mut acc = OsString::from(extra);
                    acc.push(":");
                    acc.push(prev);
                    acc
                }
                None => OsString::from(extra),
            };
            env::set_var("PATH", new_path);
            Self { original }
        }
    }

    #[cfg(unix)]
    impl Drop for PathPrepend {
        fn drop(&mut self) {
            use std::env;
            match self.original.take() {
                Some(prev) => env::set_var("PATH", prev),
                None => env::remove_var("PATH"),
            }
        }
    }

    /// Process-wide mutex serialising PATH-mutating tests so parallel
    /// test threads don't race on the `$PATH` environment variable.
    #[cfg(unix)]
    fn path_mutation_guard() -> &'static Mutex<()> {
        use std::sync::OnceLock;
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    /// Build a tempdir containing a `spin` script that emits fixed
    /// stdout/stderr and exits with the given code. Payloads are written
    /// to sidecar files so shell-active chars are never re-interpreted.
    #[cfg(unix)]
    fn fake_spin_returning(
        stdout_body: &str,
        stderr_body: &str,
        exit_code: i32,
    ) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let script_path = tmp.path().join("spin");
        let stdout_file = tmp.path().join("stdout_payload.txt");
        let stderr_file = tmp.path().join("stderr_payload.txt");
        fs::write(&stdout_file, stdout_body).expect("write stdout payload");
        fs::write(&stderr_file, stderr_body).expect("write stderr payload");
        let script = format!(
            "#!/bin/sh\ncat '{stdout}'\ncat '{stderr}' >&2\nexit {code}\n",
            stdout = stdout_file.display(),
            stderr = stderr_file.display(),
            code = exit_code,
        );
        fs::write(&script_path, &script).expect("write spin script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        tmp
    }

    /// Manifest fixture for the 8.3 tests: Spin adapter with a Fermyon
    /// Cloud deploy command so `read_config_entry` returns `Unsupported`.
    fn spin_cloud_manifest() -> String {
        r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "spin deploy"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#
        .to_owned()
    }

    /// Write the minimal `spin.toml` needed to pass `validate_adapter_manifest`.
    fn write_minimal_spin_toml(dir: &Path) {
        fs::write(
            dir.join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        )
        .expect("write spin.toml");
    }

    /// Non-TTY + no --yes + Spin Cloud (Unsupported) must error with the
    /// 8.3 non-interactive message. CI has no TTY; no --yes is passed.
    #[test]
    fn c4_unsupported_non_tty_without_yes_errors() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let (dir, manifest_path, _) = setup_project(&spin_cloud_manifest(), FIXTURE_APP_CONFIG);
        write_minimal_spin_toml(dir.path());
        let args = push_args(&manifest_path, "spin");
        // No --yes, no TTY: must error with the 8.3 non-interactive error.
        let err = run_config_push_typed::<FixtureConfig>(&args)
            .expect_err("Unsupported + non-TTY + no --yes must error");
        assert!(
            err.contains("--yes") || err.contains("non-interactive") || err.contains("unsupported"),
            "error must mention the constraint: {err}"
        );
    }

    /// `--dry-run` against Spin Cloud (Unsupported) must error immediately
    /// with the 8.3 dry-run message — the flag's contract is "show the
    /// diff", which is structurally impossible without a remote read-back.
    #[test]
    fn c4_unsupported_dry_run_errors_with_spin_cloud_message() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let (dir, manifest_path, _) = setup_project(&spin_cloud_manifest(), FIXTURE_APP_CONFIG);
        write_minimal_spin_toml(dir.path());
        let mut args = push_args(&manifest_path, "spin");
        args.dry_run = true;
        let err = run_config_push_typed::<FixtureConfig>(&args)
            .expect_err("Unsupported + --dry-run must error");
        assert!(
            err.contains("dry-run") && err.contains("unsupported"),
            "error must name both dry-run and unsupported: {err}"
        );
    }

    /// `--yes` against Spin Cloud (Unsupported) skips consent and re-fetch,
    /// writing unconditionally. The write itself will fail because the fake
    /// `spin` binary returns exit 1. The error must come from the write
    /// step, not from the consent gate.
    ///
    /// A fake `spin` binary is injected via PATH so the test is hermetic
    /// whether or not real `spin` is on the developer's machine.
    #[cfg(unix)]
    #[test]
    fn c4_unsupported_yes_flag_passes_consent_reaches_write() {
        let _manifest = manifest_guard().lock().expect("manifest guard");
        let _path = path_mutation_guard().lock().expect("path mutation guard");
        let (dir, manifest_path, _) = setup_project(&spin_cloud_manifest(), FIXTURE_APP_CONFIG);
        write_minimal_spin_toml(dir.path());
        // Inject a fake `spin` that returns exit 1 immediately so the
        // write step errors in a controlled, deterministic way without
        // stalling when real `spin` is on PATH.
        let fake = fake_spin_returning("", "Error: fake spin in test", 1);
        let _prepend = PathPrepend::new(fake.path());
        let mut args = push_args(&manifest_path, "spin");
        args.yes = true;
        // With --yes, the push must reach the write step. The fake
        // `spin` returns exit 1, so push_config_entries errors.
        // The error must NOT be the consent message.
        let err = run_config_push_typed::<FixtureConfig>(&args)
            .expect_err("Unsupported + --yes must reach the write step");
        assert!(
            !err.contains("non-interactive") && !err.contains("aborted by operator"),
            "error must come from the write step, not consent: {err}"
        );
    }

    // ---------- unified diff renderer ----------

    #[test]
    fn print_unified_diff_inline_emits_similar_format() {
        use crate::config::print_unified_diff_to_writer;
        use serde_json::json;
        let remote = json!({
            "feature": { "new_checkout": false },
            "greeting": "hello",
            "service": { "timeout_ms": 1500_i32 },
        });
        let local = json!({
            "feature": { "new_checkout": true },
            "greeting": "hello",
            "service": { "timeout_ms": 2000_i32 },
        });
        let mut buf = Vec::new();
        print_unified_diff_to_writer(&remote, &local, "aaaaaaaaXX", "bbbbbbbbXX", &mut buf)
            .unwrap();
        let output = String::from_utf8(buf).unwrap();
        // Headers carry the short SHA refs (8-char truncation).
        assert!(
            output.contains("--- remote (sha256: aaaaaaaa)"),
            "missing remote header: {output}"
        );
        assert!(
            output.contains("+++ local  (sha256: bbbbbbbb)"),
            "missing local header: {output}"
        );
        // Hunk header — git-style `@@ -<old_start>,<old_len> +<new_start>,<new_len> @@`.
        assert!(output.contains("@@ "), "missing hunk header: {output}");
        // Changed JSON lines — `-` / `+` carry the full pretty-printed line.
        assert!(
            output.contains("-    \"new_checkout\": false"),
            "missing removed line: {output}"
        );
        assert!(
            output.contains("+    \"new_checkout\": true"),
            "missing added line: {output}"
        );
        assert!(
            output.contains("-    \"timeout_ms\": 1500"),
            "missing removed timeout_ms: {output}"
        );
        assert!(
            output.contains("+    \"timeout_ms\": 2000"),
            "missing added timeout_ms: {output}"
        );
        // Unchanged context — unified-diff prefixes a space.
        assert!(
            output
                .lines()
                .any(|line| line.starts_with("   \"greeting\":")),
            "missing context line for greeting: {output}"
        );
        // Key ordering — render_for_diff sorts keys recursively so
        // `feature` < `greeting` < `service` in the output.
        let feature_pos = output.find("\"feature\"").expect("feature present");
        let greeting_pos = output.find("\"greeting\"").expect("greeting present");
        let service_pos = output.find("\"service\"").expect("service present");
        assert!(feature_pos < greeting_pos, "feature must precede greeting");
        assert!(greeting_pos < service_pos, "greeting must precede service");
    }

    #[test]
    fn print_unified_diff_inline_added_subtree_emits_multiline_block() {
        use crate::config::print_unified_diff_to_writer;
        use serde_json::json;
        // An added subtree shows as a multi-line `+` block, NOT as N
        // per-leaf `(added)` lines — the switch to `similar` over
        // hand-rolled walkers gives the multi-line block format.
        let remote = json!({ "greeting": "hello" });
        let local = json!({
            "greeting": "hello",
            "nested": { "alpha": 1_i32, "beta": 2_i32 },
        });
        let mut buf = Vec::new();
        print_unified_diff_to_writer(&remote, &local, "aa", "bb", &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        // The added subtree shows as ≥ 3 `+`-prefixed lines covering the
        // opening brace, `"alpha"`, `"beta"`, and closing brace.
        let plus_lines: Vec<&str> = output
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .collect();
        assert!(
            plus_lines.len() >= 3,
            "expected ≥ 3 added lines for the new subtree, got {plus_lines:?}"
        );
        assert!(
            plus_lines.iter().any(|line| line.contains("\"nested\"")),
            "added lines must include nested key: {plus_lines:?}"
        );
        assert!(
            plus_lines.iter().any(|line| line.contains("\"alpha\"")),
            "added lines must include alpha: {plus_lines:?}"
        );
        assert!(
            plus_lines.iter().any(|line| line.contains("\"beta\"")),
            "added lines must include beta: {plus_lines:?}"
        );
    }

    // ---------- pre-write re-fetch — file-system simulations ----------

    /// Skip-on-equal: first push writes the envelope; second push reads
    /// same envelope and exits early. The axum adapter's re-fetch reads
    /// the same on-disk file (no race), so the second read returns the
    /// same envelope as the first, triggering the concurrent-same-state
    /// skip. This covers the `c4_concurrent_push_with_same_data_skips_write`
    /// scenario using the real axum adapter.
    #[test]
    fn c4_re_fetch_skips_write_when_second_read_matches_local() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.app_config = Some(dir.path().join("demo-app.toml"));
        args.yes = true;

        // First push: MissingStore → writes. Second push: Present(same SHA).
        // On the second push: first read = Present(same SHA) → skip-on-equal
        // exits in the first match arm, BEFORE the re-fetch, so the adapter
        // write is not called a second time.
        run_config_push_typed::<FixtureConfig>(&args).expect("first push");
        run_config_push_typed::<FixtureConfig>(&args)
            .expect("second push must exit early via skip-on-equal (same SHA)");
    }

    /// When the remote has DIFFERENT content from local, --yes causes
    /// the re-fetch to read the same different content (no real race),
    /// emit the "remote changed between diff and write" warning (because
    /// `approved_remote_sha` != re-fetched SHA), and proceed to write.
    /// This verifies the re-fetch path doesn't block a legitimate write.
    #[test]
    fn c4_re_fetch_warns_and_writes_when_remote_has_different_sha() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let (dir, manifest, _) = setup_project(PUSH_MANIFEST, FIXTURE_APP_CONFIG);
        let mut args = push_args(&manifest, "axum");
        args.app_config = Some(dir.path().join("demo-app.toml"));
        args.yes = true;

        // Pre-write an envelope with different content so first read
        // returns Present with a SHA != local.
        let other_data = serde_json::json!({ "greeting": "old-state" });
        write_remote_envelope(dir.path(), "app_config", &make_envelope_json(other_data));

        // Push must succeed: re-fetch sees same different content,
        // approved_remote_sha != re-fetched SHA, so warning is emitted
        // then write proceeds.
        run_config_push_typed::<FixtureConfig>(&args)
            .expect("re-fetch with different SHA must warn and write");

        // Verify write happened (file updated with new envelope).
        let raw = fs::read_to_string(dir.path().join(".edgezero/local-config-app_config.json"))
            .expect("file must exist after write");
        assert!(
            raw.contains("greeting"),
            "written envelope has greeting: {raw}"
        );
    }

    // -------------------------------------------------------------------
    // High — diff runs run_shared_checks (adapter manifest + collision)
    // -------------------------------------------------------------------

    /// High — spec 3.3.2: `run_config_diff_typed` must run
    /// `run_shared_checks` (which includes `validate_adapter_manifest`)
    /// before reaching the remote-read step.  A broken Spin
    /// `spin.toml` (no `[component.*]` sections) triggers
    /// `validate_adapter_manifest` inside `run_adapter_shared_checks`,
    /// which must be caught even on a read-only diff.
    #[test]
    fn diff_typed_runs_shared_checks() {
        let _lock = manifest_guard().lock().expect("manifest guard");
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
        let (dir, manifest_path, _) = setup_project(manifest_spin, FIXTURE_APP_CONFIG);
        // spin.toml with ZERO components — Spin's validate_adapter_manifest
        // must reject before the function reaches the remote-read step.
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n",
        )
        .expect("write spin.toml");
        let diff_args = ConfigDiffArgs {
            adapter: "spin".to_owned(),
            app_config: None,
            exit_code: false,
            format: DiffFormat::Unified,
            key: None,
            local: false,
            manifest: manifest_path.clone(),
            no_env: true,
            runtime_config: None,
            store: None,
        };
        let err = run_config_diff_typed::<FixtureConfig>(&diff_args)
            .expect_err("missing [component.*] must fail Spin's shared-check preflight in diff");
        assert!(
            err.contains("no [component.*]") || err.contains("component"),
            "error must come from Spin's shared validate_adapter_manifest: {err}"
        );
    }

    // -------------------------------------------------------------------
    // Medium 1 — diff runs typed_secret_checks + adapter_typed_checks
    // -------------------------------------------------------------------

    /// A NESTED `#[secret]` that is present but empty must be caught by the
    /// path-aware `typed_secret_checks` on `diff` — before any remote read —
    /// and the error must name the dotted path.
    #[test]
    fn diff_typed_rejects_empty_nested_secret() {
        #[derive(Debug, Deserialize, Serialize, Validate)]
        #[serde(deny_unknown_fields)]
        struct DiffInner {
            server_side_key: String,
        }
        #[derive(Debug, Deserialize, Serialize, Validate)]
        #[serde(deny_unknown_fields)]
        struct DiffNestedConfig {
            greeting: String,
            #[validate(nested)]
            integrations: DiffInner,
        }
        impl AppConfigMeta for DiffNestedConfig {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![
                        SecretPathSegment::Field(Cow::Borrowed("integrations")),
                        SecretPathSegment::Field(Cow::Borrowed("server_side_key")),
                    ],
                    optional: false,
                }]
            }
        }

        let app_config = r#"
greeting = "hello"

[integrations]
server_side_key = ""
"#;
        let manifest = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        let (_dir, manifest_path, _) = setup_project(manifest, app_config);
        let diff_args = ConfigDiffArgs {
            adapter: "axum".to_owned(),
            app_config: None,
            exit_code: false,
            format: DiffFormat::Unified,
            key: None,
            local: false,
            manifest: manifest_path.clone(),
            no_env: true,
            runtime_config: None,
            store: None,
        };
        // The nested empty secret must be rejected by the path-aware
        // typed_secret_checks before the remote-read step, naming the path.
        let err = run_config_diff_typed::<DiffNestedConfig>(&diff_args)
            .expect_err("empty nested #[secret] must be rejected by diff typed_secret_checks");
        assert!(
            err.contains("integrations.server_side_key") && err.contains("non-empty"),
            "error names the nested dotted secret path: {err}"
        );
    }

    /// Medium 1 — spec 3.3.2: `run_config_diff_typed` must run the same
    /// structural checks as push, including `typed_secret_checks`.  A
    /// `#[secret]` field that is present but empty must be rejected even
    /// on a read-only diff operation.
    #[test]
    fn diff_typed_rejects_typed_secret_collision() {
        // Fixture config with a `#[secret]` field.  An EMPTY api_token
        // triggers `typed_secret_checks`' non-empty guard — this is the
        // simplest structural error that passes `validate_excluding_secrets`
        // but must be caught by `typed_secret_checks`.
        #[derive(Debug, Deserialize, Serialize, Validate)]
        #[serde(deny_unknown_fields)]
        struct DiffSecretConfig {
            api_token: String,
            #[validate(length(min = 1_u64))]
            greeting: String,
        }
        impl AppConfigMeta for DiffSecretConfig {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![SecretPathSegment::Field(Cow::Borrowed("api_token"))],
                    optional: false,
                }]
            }
        }

        let app_config_empty_secret = r#"
api_token = ""
greeting = "hello"
"#;
        let manifest = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        let (_dir, manifest_path, _) = setup_project(manifest, app_config_empty_secret);
        let diff_args = ConfigDiffArgs {
            adapter: "axum".to_owned(),
            app_config: None,
            exit_code: false,
            format: DiffFormat::Unified,
            key: None,
            local: false,
            manifest: manifest_path.clone(),
            no_env: true,
            runtime_config: None,
            store: None,
        };
        // typed_secret_checks must catch the empty `#[secret]` field
        // before the function reaches the remote-read step.
        let err = run_config_diff_typed::<DiffSecretConfig>(&diff_args)
            .expect_err("empty #[secret] field must be rejected by diff typed_secret_checks");
        assert!(
            err.contains("api_token") && err.contains("non-empty"),
            "error names the empty secret field: {err}"
        );
    }
}
