use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name, run_native_cli,
};
use edgezero_adapter::registry::{
    register_adapter, Adapter, AdapterAction, AdapterPushContext, ProvisionStores, ResolvedStoreId,
};
use edgezero_adapter::scaffold::{
    register_adapter_blueprint, AdapterBlueprint, AdapterFileSpec, CommandTemplates,
    DependencySpec, LoggingDefaults, ManifestSpec, ReadmeInfo, TemplateRegistration,
};
#[cfg(feature = "cli")]
use reqwest::blocking::Client as HttpClient;
use walkdir::WalkDir;

static SPIN_ADAPTER: SpinCliAdapter = SpinCliAdapter;

static SPIN_BLUEPRINT: AdapterBlueprint = AdapterBlueprint {
    id: "spin",
    display_name: "Spin (Fermyon)",
    crate_suffix: "adapter-spin",
    dependency_crate: "edgezero-adapter-spin",
    dependency_repo_path: "crates/edgezero-adapter-spin",
    template_registrations: SPIN_TEMPLATE_REGISTRATIONS,
    files: SPIN_FILE_SPECS,
    extra_dirs: &["src"],
    dependencies: SPIN_DEPENDENCIES,
    manifest: ManifestSpec {
        manifest_filename: "spin.toml",
        build_target: "wasm32-wasip2",
        build_profile: "release",
        build_features: &["spin"],
    },
    commands: CommandTemplates {
        build: "cargo build --target wasm32-wasip2 --release -p {crate}",
        deploy: "spin deploy --from {crate_dir}",
        serve: "spin up --from {crate_dir} --runtime-config-file {crate_dir}/runtime-config.toml",
    },
    logging: LoggingDefaults {
        endpoint: None,
        level: "info",
        echo_stdout: None,
    },
    readme: ReadmeInfo {
        description: "{display} entrypoint.",
        dev_heading: "{display} (local)",
        dev_steps: &["`edgezero serve --adapter spin`"],
    },
    run_module: "edgezero_adapter_spin",
};

static SPIN_DEPENDENCIES: &[DependencySpec] = &[
    DependencySpec {
        key: "dep_edgezero_core_spin",
        repo_crate: "crates/edgezero-core",
        fallback: "edgezero-core = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_spin",
        repo_crate: "crates/edgezero-adapter-spin",
        fallback:
            "edgezero-adapter-spin = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-spin\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_spin_wasm",
        repo_crate: "crates/edgezero-adapter-spin",
        fallback:
            "edgezero-adapter-spin = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-spin\", default-features = false, features = [\"spin\"] }",
        features: &["spin"],
    },
];

static SPIN_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "spin_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "spin_runtime_config_toml",
        output: "runtime-config.toml",
    },
    AdapterFileSpec {
        template: "spin_src_lib_rs",
        output: "src/lib.rs",
    },
    AdapterFileSpec {
        template: "spin_spin_toml",
        output: "spin.toml",
    },
];

static SPIN_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "spin_Cargo_toml",
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "spin_runtime_config_toml",
        contents: include_str!("templates/runtime-config.toml.hbs"),
    },
    TemplateRegistration {
        name: "spin_src_lib_rs",
        contents: include_str!("templates/src/lib.rs.hbs"),
    },
    TemplateRegistration {
        name: "spin_spin_toml",
        contents: include_str!("templates/spin.toml.hbs"),
    },
];

const TARGET_TRIPLE: &str = "wasm32-wasip2";

const SPIN_INSTALL_HINT: &str = "install the Spin CLI (https://spinframework.dev/) and try again";

struct SpinCliAdapter;

#[expect(
    clippy::missing_trait_methods,
    reason = "Stage 6: KV-backed config dropped Spin's `^[a-z][a-z0-9_]*$` key rule and the config-vs-secret collision check, so `validate_app_config_keys` falls back to the trait default `Ok(())`. `validate_typed_secrets` IS overridden below (secret-value canonicalisation + within-secrets uniqueness still apply). `validate_adapter_manifest` IS overridden below (Spin's multi-component disambiguation)."
)]
impl Adapter for SpinCliAdapter {
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            // `spin cloud {login|logout|info}` is the native sign-in
            // surface for Fermyon Cloud. EdgeZero stores no
            // credentials — this is a thin shell-out.
            AdapterAction::AuthLogin => {
                run_native_cli("spin", &["cloud", "login"], SPIN_INSTALL_HINT)
            }
            AdapterAction::AuthLogout => {
                run_native_cli("spin", &["cloud", "logout"], SPIN_INSTALL_HINT)
            }
            AdapterAction::AuthStatus => {
                run_native_cli("spin", &["cloud", "info"], SPIN_INSTALL_HINT)
            }
            AdapterAction::Build => {
                let artifact = build(args)?;
                log::info!("[edgezero] Spin build complete -> {}", artifact.display());
                Ok(())
            }
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
            other => Err(format!("spin adapter does not support {other:?}")),
        }
    }

    fn name(&self) -> &'static str {
        "spin"
    }

    fn provision(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        stores: &ProvisionStores<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        //: spin provision is pure spin.toml editing — no
        // shell-out (Spin KV stores are provisioned by the Spin
        // runtime / Fermyon at deploy). For each declared KV id
        // AND each declared CONFIG id (KV-backed since Stage 5
        // of the spin-kv-config plan), append the env-resolved
        // platform label to the component's `key_value_stores`
        // array. Secret variables are manually declared by the
        // developer in spin.toml -- secrets stay on Spin
        // variables for the platform's `secret = true` flagging.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.spin.adapter].manifest must point at spin.toml for provision".to_owned(),
            );
        };
        let spin_path = manifest_root.join(rel);

        let mut out = Vec::new();
        // Resolve the component once if either KV or config has
        // anything to provision.
        let needs_component = !stores.kv.is_empty() || !stores.config.is_empty();
        if needs_component {
            let component_id = resolve_spin_component(&spin_path, component_selector)?;
            for (kind, store) in stores
                .kv
                .iter()
                .map(|store| ("KV", store))
                .chain(stores.config.iter().map(|store| ("config", store)))
            {
                let logical = store.logical.as_str();
                // The label the runtime opens is what
                // `EDGEZERO__STORES__<KIND>__<LOGICAL>__NAME`
                // resolves to (default = the logical id). Provision
                // writes the PLATFORM label into
                // `[component.X].key_value_stores` so that both the
                // KV runtime lookup AND the KV-backed config
                // runtime lookup match.
                let label = store.platform.as_str();
                if dry_run {
                    out.push(format!(
                        "would ensure {kind} label `{label}` (logical id `{logical}`) is in [component.{component_id}].key_value_stores in {}",
                        spin_path.display()
                    ));
                    continue;
                }
                let added = ensure_kv_label_in_component(&spin_path, &component_id, label)?;
                if added {
                    out.push(format!(
                        "added {kind} label `{label}` (logical id `{logical}`) to [component.{component_id}].key_value_stores in {}",
                        spin_path.display()
                    ));
                } else {
                    out.push(format!(
                        "{kind} label `{label}` (logical id `{logical}`) already present in [component.{component_id}].key_value_stores in {}; skipping",
                        spin_path.display()
                    ));
                }
            }
        }
        for store in stores.secrets {
            let logical = store.logical.as_str();
            let platform = store.platform.as_str();
            out.push(format!(
                "spin secret id `{logical}` (platform name `{platform}`) requires manual `[variables].* secret = true` + `[component.*.variables].*` declarations in spin.toml; nothing to do here"
            ));
        }
        if out.is_empty() {
            out.push("spin has no declared stores to provision".to_owned());
        }
        Ok(out)
    }

    fn push_config_entries(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        entries: &[(String, String)],
        push_ctx: &AdapterPushContext<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // Stage 4: HTTP POST to the seed handler at `push_ctx.seed_url`.
        // The CLI's load_push_context (D8) resolves the URL through
        // the prod or local chain (per D3) and stashes it in
        // `push_ctx.seed_url`. The body's `store` is the platform
        // label (NOT logical id) so an operator with
        // `EDGEZERO__STORES__CONFIG__<ID>__NAME=…` set sees the
        // matching label flow through. See D9 + D12 for the
        // request/response contract.
        let platform = store.platform.as_str();
        let logical = store.logical.as_str();

        if entries.is_empty() {
            return Ok(vec![format!(
                "no config entries to push to spin store `{platform}` (logical id `{logical}`)"
            )]);
        }

        let Some(seed_url) = push_ctx.seed_url else {
            return Err(format!(
                "seed URL is not configured for spin push: pass `--seed-url <url>`, set `EDGEZERO__ADAPTERS__SPIN__SEED_URL`{}, or add `[adapters.spin.commands].seed_url` to edgezero.toml",
                if push_ctx.local { " / `EDGEZERO__ADAPTERS__SPIN__LOCAL_SEED_URL`" } else { "" }
            ));
        };

        if dry_run {
            let mut out = Vec::with_capacity(entries.len().saturating_add(1));
            out.push(format!(
                "would POST {entries_n} entries to {seed_url} for store `{platform}` (logical id `{logical}`):",
                entries_n = entries.len(),
            ));
            for (key, _) in entries {
                out.push(format!("  would set `{key}`"));
            }
            return Ok(out);
        }

        let Some(seed_token) = push_ctx.seed_token else {
            return Err(
                "seed token is not configured for spin push: pass `--seed-token <token>` or set `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN` (tokens are NEVER read from edgezero.toml)"
                    .to_owned(),
            );
        };

        let payload = build_seed_payload(platform, entries);
        let body = serde_json::to_vec(&payload)
            .map_err(|err| format!("failed to serialize seed payload as JSON: {err}"))?;

        let client = HttpClient::new();
        let response = client
            .post(seed_url)
            .header("content-type", "application/json")
            .header("x-edgezero-seed", seed_token)
            .body(body)
            .send()
            .map_err(|err| {
                if err.is_connect() {
                    format!(
                        "seed POST to {seed_url} failed: connection refused. Is the Spin app running?"
                    )
                } else {
                    format!("seed POST to {seed_url} failed: {err}")
                }
            })?;

        let status = response.status();
        let response_text = response.text().unwrap_or_default();
        // D9 status code table → D12 message table.
        match status.as_u16() {
            204 => Ok(vec![format!(
                "pushed {} entries to spin store `{platform}` (logical id `{logical}`) via {seed_url}",
                entries.len()
            )]),
            400 => Err(format!(
                "seed handler rejected (400 Bad Request): {response_text}. Check CLI version / store id."
            )),
            401 => Err(
                "seed handler rejected (401 Unauthorized). Fail-closed reasons (D9): server-side `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN` is unset, blank, whitespace-only, or shorter than 16 bytes; OR your `--seed-token` / `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN` is missing. Check the server's env first -- a 4-character placeholder triggers this even when the wire token matches.".to_owned(),
            ),
            403 => Err(
                "seed handler rejected (403 Forbidden): x-edgezero-seed mismatch. Check that the token on the client matches the server's EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN.".to_owned(),
            ),
            404 => Err(format!(
                "seed handler rejected (404 Not Found): store `{platform}` is not a recognised platform label. Check `[stores.config].ids` and any `EDGEZERO__STORES__CONFIG__<ID>__NAME` overrides."
            )),
            405 => Err(
                "seed handler rejected (405 Method Not Allowed). This usually means a transparent proxy rewrote the POST -- check intermediaries.".to_owned(),
            ),
            415 => Err(
                "seed handler rejected (415 Unsupported Media Type). Internal: the CLI should always set content-type: application/json.".to_owned(),
            ),
            422 => Err(format!(
                "seed handler rejected (422 Unprocessable): KV write failed mid-stream: {response_text}"
            )),
            other => Err(format!(
                "seed handler returned unexpected status {other}: {response_text}"
            )),
        }
    }

    fn push_config_entries_local(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        store: &ResolvedStoreId,
        entries: &[(String, String)],
        push_ctx: &AdapterPushContext<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // Stage 4: the local URL is already resolved in `push_ctx.seed_url`
        // by the CLI's load_push_context (D3 local chain: --seed-url ->
        // EDGEZERO__ADAPTERS__SPIN__LOCAL_SEED_URL -> builtin
        // http://127.0.0.1:3000/__edgezero/config/seed). The implementation
        // is identical to the prod push from this side; the URL chain
        // already encoded "local" semantics.
        self.push_config_entries(
            manifest_root,
            adapter_manifest_path,
            component_selector,
            store,
            entries,
            push_ctx,
            dry_run,
        )
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        //: Multi for KV AND Config (both label-backed via the
        // Spin KV API since Stage 5 of the spin-kv-config plan).
        // Single for Secrets (still flat-variable namespace).
        &["secrets"]
    }

    fn validate_adapter_manifest(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
    ) -> Result<(), String> {
        // check 3: spin.toml must exist and either declare
        // exactly one `[component.*]` or carry an explicit selector
        // that matches one of the declared ids.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.spin.adapter].manifest must point at spin.toml for Spin component discovery".to_owned()
            );
        };
        let spin_path = manifest_root.join(rel);
        let raw = fs::read_to_string(&spin_path).map_err(|err| {
            format!(
                "failed to read spin manifest at {}: {err}",
                spin_path.display()
            )
        })?;
        let parsed: toml::Value = toml::from_str(&raw)
            .map_err(|err| format!("failed to parse {} as TOML: {err}", spin_path.display()))?;
        let component_ids = collect_spin_component_ids(&parsed);

        if component_ids.is_empty() {
            return Err(format!(
                "{}: no [component.*] declarations found",
                spin_path.display()
            ));
        }

        if let Some(selector) = component_selector {
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

    fn validate_typed_secrets(
        &self,
        _config_keys: &[&str],
        plain_secrets: &[(&str, &str)],
    ) -> Result<(), String> {
        // Stage 5+: KV-backed config no longer shares Spin's flat
        // variable namespace, so `config_keys` are NOT considered
        // here — config can use arbitrary UTF-8 keys without
        // colliding with `#[secret]` values. Secrets still resolve
        // through `spin_sdk::variables`, so two checks remain:
        //   1. each `#[secret]` value canonicalises (lowercase, no
        //      `.→__` — secrets don't get translated at runtime)
        //      to a valid Spin variable name, so invalid chars
        //      (dashes, digit-first) fail validation rather than
        //      at runtime with an opaque `InvalidName`;
        //   2. no two `#[secret]` values collapse to the same
        //      lowercased Spin variable, since Spin's flat
        //      namespace cannot disambiguate them.
        let mut seen: HashSet<String> = HashSet::with_capacity(plain_secrets.len());
        for (field_name, value) in plain_secrets {
            let spin_var = value.to_ascii_lowercase();
            if !is_valid_spin_key(&spin_var) {
                let reason = spin_key_rule_violation(&spin_var);
                return Err(format!(
                    "`#[secret]` field `{field_name}` value `{value}` translates to Spin variable `{spin_var}`, which is not a valid Spin variable name. {reason}. Pick a `#[secret]` value that conforms."
                ));
            }
            if !seen.insert(spin_var.clone()) {
                return Err(format!(
                    "Spin variable `{spin_var}` (from `#[secret]` field `{field_name}`) collides with another `#[secret]` value resolving to the same lowercased name; Spin's flat variable namespace cannot disambiguate them"
                ));
            }
        }
        Ok(())
    }
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

/// Return a per-failure-mode diagnostic for a key that failed
/// `is_valid_spin_key`. Spin's variable-name rule
/// (`^[a-z][a-z0-9_]*$`) is one regex but the operator usually
/// wants to know WHICH bit they broke: digit-leading, uppercase,
/// or stray punctuation. Returns a short phrase to splice into
/// the caller's full error.
fn spin_key_rule_violation(key: &str) -> &'static str {
    // Callers only invoke this AFTER `is_valid_spin_key` returned
    // false; in production the per-char branches below exhaust the
    // failure modes and the catch-all at the bottom is unreachable.
    // It's kept defensively so a future regex tweak (e.g. allowing
    // a new char class) doesn't crash the diagnostic helper with
    // an unreachable!() before the caller can produce its error.
    //
    // Reachability notes for the per-mode branches:
    // - `push_config_entries` translates keys via
    //   `translate_key_for_spin` (which lowercases) BEFORE this
    //   call, so the uppercase-first branch is unreachable from
    //   that site. It IS reachable from `validate_app_config_keys`
    //   and `validate_typed_secrets`, which check raw user input.
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return "Spin variable names must not be empty";
    };
    if first.is_ascii_digit() {
        return "Spin variable names must start with a lowercase letter, not a digit";
    }
    if first.is_ascii_uppercase() {
        return "Spin variable names must be lowercase (uppercase letters are not allowed)";
    }
    if !first.is_ascii_lowercase() {
        return "Spin variable names must start with a lowercase ASCII letter";
    }
    for ch in chars {
        if ch.is_ascii_uppercase() {
            return "Spin variable names must be lowercase (uppercase letters are not allowed)";
        }
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_') {
            return "Spin variable names may only contain lowercase letters, digits, and underscores";
        }
    }
    debug_assert!(
        false,
        "spin_key_rule_violation called with key `{key}` that satisfies the regex; check is_valid_spin_key + caller agreement"
    );
    "Spin variable names must match `^[a-z][a-z0-9_]*$`"
}

fn collect_spin_component_ids(parsed: &toml::Value) -> Vec<String> {
    parsed
        .as_table()
        .and_then(|root| root.get("component"))
        .and_then(toml::Value::as_table)
        .map(|components| components.keys().cloned().collect())
        .unwrap_or_default()
}

/// Resolve which `[component.<id>]` table `provision` should
/// write into. Mirrors the rule used by `validate_adapter_manifest`
/// Build the seed handler JSON body per D9 schema.
///
/// `platform` is the env-resolved platform label (NOT the logical
/// id). The handler validates `body.store` against the set of
/// labels computed from `A::stores().config × env.store_name`.
fn build_seed_payload(platform: &str, entries: &[(String, String)]) -> serde_json::Value {
    let entries_json: Vec<serde_json::Value> = entries
        .iter()
        .map(|(key, value)| {
            serde_json::json!({
                "key": key,
                "value": value,
            })
        })
        .collect();
    serde_json::json!({
        "store": platform,
        "entries": entries_json,
    })
}

///: single-component spin.toml resolves implicitly,
/// multi-component requires an explicit `component = "..."` in
/// `[adapters.spin.adapter]`, and a selector that doesn't match
/// any declared id is an error.
fn resolve_spin_component(
    spin_path: &Path,
    component_selector: Option<&str>,
) -> Result<String, String> {
    let raw = fs::read_to_string(spin_path).map_err(|err| {
        format!(
            "failed to read spin manifest at {}: {err}",
            spin_path.display()
        )
    })?;
    let parsed: toml::Value = toml::from_str(&raw)
        .map_err(|err| format!("failed to parse {} as TOML: {err}", spin_path.display()))?;
    let component_ids = collect_spin_component_ids(&parsed);

    if component_ids.is_empty() {
        return Err(format!(
            "{}: no [component.*] declarations found",
            spin_path.display()
        ));
    }
    if let Some(selector) = component_selector {
        if component_ids.iter().any(|id| id == selector) {
            return Ok(selector.to_owned());
        }
        return Err(format!(
            "[adapters.spin.adapter].component = {:?} is not declared in {} (available: {})",
            selector,
            spin_path.display(),
            component_ids.join(", ")
        ));
    }
    if component_ids.len() == 1 {
        return Ok(component_ids.into_iter().next().unwrap_or_default());
    }
    Err(format!(
        "{} declares {} components ({}) but [adapters.spin.adapter].component is unset; set one explicitly",
        spin_path.display(),
        component_ids.len(),
        component_ids.join(", ")
    ))
}

/// Ensure `label` appears in `[component.<component_id>]`'s
/// `key_value_stores = [...]` array. Creates the array if absent.
/// Returns `Ok(true)` if the label was newly added, `Ok(false)` if
/// it was already there (idempotent across re-runs). Preserves the
/// rest of the spin manifest, including formatting and comments.
fn ensure_kv_label_in_component(
    spin_path: &Path,
    component_id: &str,
    label: &str,
) -> Result<bool, String> {
    use toml_edit::{value, Array, DocumentMut, Value};

    let raw = fs::read_to_string(spin_path)
        .map_err(|err| format!("failed to read {}: {err}", spin_path.display()))?;
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", spin_path.display()))?;

    let component_root = doc.get_mut("component").ok_or_else(|| {
        format!(
            "{}: [component.*] tables expected but `component` key missing",
            spin_path.display()
        )
    })?;
    let component_tbl = component_root
        .as_table_mut()
        .ok_or_else(|| format!("{}: `component` is not a table", spin_path.display()))?;
    let target = component_tbl.get_mut(component_id).ok_or_else(|| {
        format!(
            "{}: [component.{component_id}] is not declared",
            spin_path.display()
        )
    })?;
    let target_tbl = target.as_table_mut().ok_or_else(|| {
        format!(
            "{}: [component.{component_id}] is not a table",
            spin_path.display()
        )
    })?;

    let entry = target_tbl
        .entry("key_value_stores")
        .or_insert_with(|| value(Array::new()));
    let arr = entry
        .as_value_mut()
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            format!(
                "{}: [component.{component_id}].key_value_stores is not an array",
                spin_path.display()
            )
        })?;

    if arr.iter().any(|item| item.as_str() == Some(label)) {
        return Ok(false);
    }
    arr.push(label);

    fs::write(spin_path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", spin_path.display()))?;
    Ok(true)
}

/// # Errors
/// Returns an error if the Spin CLI build command fails.
#[inline]
pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;
    let cargo_manifest = manifest_dir.join("Cargo.toml");
    let crate_name = read_package_name(&cargo_manifest)?;

    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            TARGET_TRIPLE,
            "--manifest-path",
            cargo_manifest
                .to_str()
                .ok_or("invalid Cargo manifest path")?,
        ])
        .args(extra_args)
        .status()
        .map_err(|err| format!("failed to run cargo build: {err}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with status {status}"));
    }

    let workspace_root = find_workspace_root(manifest_dir);
    let artifact = locate_artifact(&workspace_root, manifest_dir, &crate_name)?;
    let pkg_dir = workspace_root.join("pkg");
    fs::create_dir_all(&pkg_dir)
        .map_err(|err| format!("failed to create {}: {err}", pkg_dir.display()))?;
    let dest = pkg_dir.join(format!("{}.wasm", crate_name.replace('-', "_")));
    fs::copy(&artifact, &dest)
        .map_err(|err| format!("failed to copy artifact to {}: {err}", dest.display()))?;

    Ok(dest)
}

/// # Errors
/// Returns an error if the Spin CLI deploy command fails.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;

    let status = Command::new("spin")
        .args(["deploy"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run spin CLI: {err}"))?;
    if !status.success() {
        return Err(format!("spin deploy failed with status {status}"));
    }

    Ok(())
}

fn find_spin_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "spin.toml") {
        return Ok(found);
    }

    let root = find_workspace_root(start);
    let mut candidates: Vec<PathBuf> = WalkDir::new(&root)
        .follow_links(true)
        .max_depth(8)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.path().to_path_buf())
        .filter(|path| {
            path.file_name().is_some_and(|n| n == "spin.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate spin.toml".to_owned());
    }

    candidates.sort_by_key(|path| {
        let parent = path.parent().unwrap_or(Path::new(""));
        path_distance(start, parent)
    });

    Ok(candidates.remove(0))
}

fn locate_artifact(
    workspace_root: &Path,
    manifest_dir: &Path,
    crate_name: &str,
) -> Result<PathBuf, String> {
    let release_name = format!("{}.wasm", crate_name.replace('-', "_"));

    if let Some(custom) = env::var_os("CARGO_TARGET_DIR") {
        let candidate = PathBuf::from(custom)
            .join(TARGET_TRIPLE)
            .join("release")
            .join(&release_name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    let manifest_target = manifest_dir
        .join("target")
        .join(TARGET_TRIPLE)
        .join("release")
        .join(&release_name);
    if manifest_target.exists() {
        return Ok(manifest_target);
    }

    let workspace_target = workspace_root
        .join("target")
        .join(TARGET_TRIPLE)
        .join("release")
        .join(&release_name);
    if workspace_target.exists() {
        return Ok(workspace_target);
    }

    Err(format!(
        "compiled artifact not found (looked in {} and workspace target)",
        manifest_dir.display()
    ))
}

#[inline]
pub fn register() {
    register_adapter(&SPIN_ADAPTER);
    register_adapter_blueprint(&SPIN_BLUEPRINT);
}

#[ctor(unsafe)]
fn register_ctor() {
    register();
}

/// # Errors
/// Returns an error if the Spin CLI up command fails.
#[inline]
pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;

    let status = Command::new("spin")
        .args(["up"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run spin CLI: {err}"))?;
    if !status.success() {
        return Err(format!("spin up failed with status {status}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Shared fixture names. Pinning these as consts (instead of
    // inline `"sessions"` / `"app_config"` / `"demo"` per call site)
    // keeps the setup-vs-assertion pair in sync -- a typo in one
    // place no longer silently divorces from the other, because both
    // reference the same const. Also names the intent: these are the
    // LOGICAL store ids + spin component id the adapter operates on,
    // not arbitrary strings.
    const TEST_KV_ID: &str = "sessions";
    const TEST_KV_ID_ALT: &str = "cache";
    const TEST_CONFIG_ID: &str = "app_config";
    const TEST_SECRET_ID: &str = "default";
    const TEST_COMPONENT_ID: &str = "demo";

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
    fn spin_key_rule_violation_picks_the_right_diagnostic_per_mode() {
        // Pin the exact diagnostic string per failure mode so a
        // future branch reorder can't pass these assertions by
        // accident (e.g. "lowercase" appears in two distinct return
        // values, so a substring-only check was too lax).
        assert_eq!(
            spin_key_rule_violation(""),
            "Spin variable names must not be empty"
        );
        assert_eq!(
            spin_key_rule_violation("1foo"),
            "Spin variable names must start with a lowercase letter, not a digit"
        );
        assert_eq!(
            spin_key_rule_violation("Foo"),
            "Spin variable names must be lowercase (uppercase letters are not allowed)"
        );
        assert_eq!(
            spin_key_rule_violation("foo-bar"),
            "Spin variable names may only contain lowercase letters, digits, and underscores"
        );
        assert_eq!(
            spin_key_rule_violation("fooBar"),
            "Spin variable names must be lowercase (uppercase letters are not allowed)"
        );
        // `_foo` starts with `_` -- not digit, not uppercase, not
        // lowercase ASCII letter. Falls through to the "must start
        // with a lowercase ASCII letter" branch.
        assert_eq!(
            spin_key_rule_violation("_foo"),
            "Spin variable names must start with a lowercase ASCII letter"
        );
    }

    #[test]
    fn validate_typed_secrets_passes_with_no_collision() {
        SpinCliAdapter
            .validate_typed_secrets(
                &["greeting", "service.timeout_ms"],
                &[("api_token", "demo_api_token")],
            )
            .expect("non-colliding inputs must pass");
    }

    /// `#[secret]` values must also be valid Spin variable names
    /// after canonicalisation. A dashed value like `"api-token"`
    /// reaches Spin at runtime and gets rejected with an opaque
    /// `InvalidName` — the validator should catch it earlier.
    #[test]
    fn validate_typed_secrets_rejects_invalid_spin_variable_in_secret_value() {
        let err = SpinCliAdapter
            .validate_typed_secrets(&["greeting"], &[("api_token", "api-token")])
            .expect_err("dashed secret value must error");
        assert!(
            err.contains("api-token") && err.contains("api-token") && err.contains("Spin variable"),
            "error names the bad value + that it's a Spin variable issue: {err}"
        );
    }

    /// Negative case: a lowercased secret value that happens to
    /// coincide with another lowercased value MUST collide
    /// (sanity check that the `seen` set still works post-fix).
    #[test]
    fn validate_typed_secrets_detects_collision_between_two_lowercased_secret_values() {
        let err = SpinCliAdapter
            .validate_typed_secrets(&[], &[("first", "SHARED_NAME"), ("second", "shared_name")])
            .expect_err("two values lowercasing to the same name must collide");
        assert!(
            err.contains("shared_name") && err.contains("collides"),
            "error names the shared canonical name: {err}"
        );
    }

    #[test]
    fn validate_adapter_manifest_errors_on_zero_components() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n",
        )
        .unwrap();
        let err = SpinCliAdapter
            .validate_adapter_manifest(dir.path(), Some("spin.toml"), None)
            .expect_err("no [component.*] must error");
        assert!(
            err.contains("no [component.*]"),
            "error explains the absence: {err}"
        );
    }

    #[test]
    fn validate_adapter_manifest_rejects_bad_selector_against_single_component() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.actual]\nsource = \"a.wasm\"\n",
        )
        .unwrap();
        let err = SpinCliAdapter
            .validate_adapter_manifest(dir.path(), Some("spin.toml"), Some("typo"))
            .expect_err("typo selector must error");
        assert!(
            err.contains("typo") && err.contains("actual"),
            "error names both the bad selector and the available id: {err}"
        );
    }

    #[test]
    fn single_store_kinds_is_secrets_only() {
        // Stage 5: config moved to KV (provisioned via `key_value_stores`,
        // entries pushed via the seed handler). Secrets remain Spin
        // `[variables]` until we ship native secret support.
        assert_eq!(SpinCliAdapter.single_store_kinds(), &["secrets"]);
    }

    #[test]
    fn finds_closest_manifest_when_multiple_exist() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        let first = root.join("crates/first");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("Cargo.toml"), "[package]\nname=\"first\"").unwrap();
        fs::write(first.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let second = root.join("examples/second");
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("Cargo.toml"), "[package]\nname=\"second\"").unwrap();
        fs::write(second.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let found = find_spin_manifest(&second).unwrap();
        assert_eq!(found, second.join("spin.toml"));
    }

    #[test]
    fn finds_manifest_in_current_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(root.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let manifest = find_spin_manifest(root).expect("should find manifest");
        assert_eq!(manifest, root.join("spin.toml"));
    }

    #[test]
    fn locate_artifact_considers_workspace_target() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest_dir = workspace.join("service");
        fs::create_dir_all(manifest_dir.join("target/wasm32-wasip2/release")).unwrap();
        let artifact = workspace.join("target/wasm32-wasip2/release/demo.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let located = locate_artifact(workspace, &manifest_dir, TEST_COMPONENT_ID).unwrap();
        assert_eq!(located, artifact);
    }

    #[test]
    fn locate_artifact_converts_hyphens_to_underscores() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest_dir = workspace.join("crates/my-cool-crate");
        fs::create_dir_all(&manifest_dir).unwrap();

        // Cargo emits underscored filenames for hyphenated crate names.
        let artifact = workspace.join("target/wasm32-wasip2/release/my_cool_crate.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let located = locate_artifact(workspace, &manifest_dir, "my-cool-crate").unwrap();
        assert_eq!(located, artifact);
    }

    // ---------- resolve_spin_component ----------

    fn write_spin(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("spin.toml");
        fs::write(&path, contents).expect("write spin.toml");
        path
    }

    #[test]
    fn resolve_spin_component_picks_single_component_implicitly() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.only]\nsource = \"a.wasm\"\n",
        );
        let resolved = resolve_spin_component(&path, None).expect("resolve");
        assert_eq!(resolved, "only");
    }

    #[test]
    fn resolve_spin_component_uses_selector_when_present() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.first]\nsource = \"a.wasm\"\n[component.second]\nsource = \"b.wasm\"\n",
        );
        let resolved = resolve_spin_component(&path, Some("second")).expect("resolve");
        assert_eq!(resolved, "second");
    }

    #[test]
    fn resolve_spin_component_errors_on_multi_without_selector() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.first]\nsource = \"a.wasm\"\n[component.second]\nsource = \"b.wasm\"\n",
        );
        let err = resolve_spin_component(&path, None).expect_err("ambiguous must error");
        assert!(
            err.contains("first") && err.contains("second"),
            "error lists candidates: {err}"
        );
    }

    #[test]
    fn resolve_spin_component_errors_on_bad_selector() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.real]\nsource = \"a.wasm\"\n",
        );
        let err = resolve_spin_component(&path, Some("typo")).expect_err("bad selector must error");
        assert!(
            err.contains("typo") && err.contains("real"),
            "error names bad selector and available id: {err}"
        );
    }

    // ---------- ensure_kv_label_in_component ----------

    #[test]
    fn ensure_kv_label_adds_to_component_without_key_value_stores() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let added =
            ensure_kv_label_in_component(&path, TEST_COMPONENT_ID, TEST_KV_ID).expect("ensure");
        assert!(added, "newly added label should return true");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("key_value_stores = [\"sessions\"]")
                || after.contains("key_value_stores = ['sessions']"),
            "added KV label: {after}"
        );
    }

    #[test]
    fn ensure_kv_label_appends_to_existing_array() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\nkey_value_stores = [\"cache\"]\n",
        );
        let added =
            ensure_kv_label_in_component(&path, TEST_COMPONENT_ID, TEST_KV_ID).expect("ensure");
        assert!(added);
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("\"cache\""), "kept existing label: {after}");
        assert!(after.contains("\"sessions\""), "added new label: {after}");
    }

    #[test]
    fn ensure_kv_label_is_idempotent_when_already_present() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\nkey_value_stores = [\"sessions\"]\n",
        );
        let added =
            ensure_kv_label_in_component(&path, TEST_COMPONENT_ID, TEST_KV_ID).expect("ensure");
        assert!(!added, "duplicate label should return false");
    }

    #[test]
    fn ensure_kv_label_errors_when_component_missing() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let err = ensure_kv_label_in_component(&path, "missing", TEST_KV_ID)
            .expect_err("missing component must error");
        assert!(
            err.contains("missing"),
            "error names the missing component id: {err}"
        );
    }

    #[test]
    fn ensure_kv_label_preserves_top_comments_and_other_fields() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "# keep me\nspin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\nallowed_outbound_hosts = []\n",
        );
        ensure_kv_label_in_component(&path, TEST_COMPONENT_ID, TEST_KV_ID).expect("ensure");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("# keep me"), "preserved comment: {after}");
        assert!(
            after.contains("allowed_outbound_hosts = []"),
            "preserved sibling field: {after}"
        );
    }

    // ---------- provision (dry-run + error path + idempotent skip) ----------

    #[test]
    fn provision_dry_run_does_not_edit_spin_toml() {
        let dir = tempdir().expect("tempdir");
        let original =
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n";
        let path = write_spin(dir.path(), original);
        let kv_ids: Vec<ResolvedStoreId> =
            ResolvedStoreId::from_logicals(&[TEST_KV_ID, TEST_KV_ID_ALT]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, true)
            .expect("dry-run succeeds");
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("would ensure KV label `sessions`"));
        assert!(out[1].contains("would ensure KV label `cache`"));
        let after = fs::read_to_string(&path).expect("read back");
        assert_eq!(after, original, "dry-run mutated spin.toml");
    }

    #[test]
    fn provision_writes_resolved_platform_label_into_kv_array() {
        // Regression: spin provision used to receive only logical
        // ids and add them verbatim to
        // `[component.X].key_value_stores`. With the platform-name
        // flow, an operator who sets
        // `EDGEZERO__STORES__KV__SESSIONS__NAME=prod_sessions` now
        // sees `prod_sessions` land as the KV label (matching what
        // the runtime opens), with the logical id preserved for
        // human-facing wording.
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let kv_ids = vec![ResolvedStoreId::new(TEST_KV_ID, "prod_sessions")];
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("real-run succeeds");
        assert!(
            out[0].contains("`prod_sessions`") && out[0].contains("`sessions`"),
            "status line names BOTH the platform label and the logical id: {out:?}"
        );

        let after = fs::read_to_string(&path).expect("read spin.toml");
        assert!(
            after.contains("\"prod_sessions\""),
            "platform label written into spin.toml KV array: {after}"
        );
        assert!(
            !after.contains("\"sessions\""),
            "logical id is NOT written (would shadow the platform binding): {after}"
        );
    }

    #[test]
    fn provision_writes_kv_labels_into_resolved_component() {
        let dir = tempdir().expect("tempdir");
        write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("real run succeeds");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("added KV label `sessions`"), "got: {out:?}");
        let after = fs::read_to_string(dir.path().join("spin.toml")).expect("read back");
        assert!(
            after.contains("\"sessions\""),
            "label landed in spin.toml: {after}"
        );
    }

    #[test]
    fn provision_errors_when_adapter_manifest_path_missing() {
        let dir = tempdir().expect("tempdir");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = SpinCliAdapter
            .provision(dir.path(), None, None, &stores, true)
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("spin.toml"),
            "error names what's missing: {err}"
        );
    }

    #[test]
    fn provision_writes_config_labels_into_kv_array_and_leaves_secrets_manual() {
        // Stage 5: config now lives in Spin KV. Provision writes each
        // `[stores.config].id` into `[component.X].key_value_stores`
        // (same machinery as `[stores.kv]`). Secrets stay manual until
        // we ship native secret support.
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let secret_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_SECRET_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &secret_ids,
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("config + secrets provision succeeds");
        assert_eq!(out.len(), 2);
        assert!(
            out[0].contains("config label") && out[0].contains("key_value_stores"),
            "config row reports KV-array write: {out:?}"
        );
        assert!(
            out[1].contains("manual"),
            "secret row still flags manual declaration: {out:?}"
        );

        let after = fs::read_to_string(&path).expect("read spin.toml");
        assert!(
            after.contains(&format!("\"{TEST_CONFIG_ID}\"")),
            "config label landed in spin.toml: {after}"
        );
    }

    #[test]
    fn provision_with_no_declared_stores_says_so() {
        let dir = tempdir().expect("tempdir");
        write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("no-store provision is fine");
        assert_eq!(out, vec!["spin has no declared stores to provision"]);
    }

    // ---------- push_config_entries (Stage 4: HTTP POST to seed handler) ----------
    //
    // The variables-backed dry-run / write / key-validation / dashed-key tests
    // that lived here before Stage 4 were deleted: they asserted spin.toml
    // editing + `.→__` translation, neither of which the KV-backed push does.
    // T4.7 in the plan calls for the new tests below (dry-run shape, missing-
    // seed-url / token errors, JSON body shape). HTTP integration coverage
    // lives in the Stage 8 `spin up` smoke test.

    fn config_store(logical: &str) -> ResolvedStoreId {
        ResolvedStoreId::from_logical(logical)
    }

    #[test]
    fn push_with_no_entries_reports_no_op_without_posting() {
        // Zero entries short-circuits before any seed-url lookup -- handy
        // when a typed AppConfig strips all `#[secret]` fields.
        let dir = tempdir().expect("tempdir");
        let out = SpinCliAdapter
            .push_config_entries(
                dir.path(),
                Some("spin.toml"),
                None,
                &config_store(TEST_CONFIG_ID),
                &[],
                &AdapterPushContext::new(),
                false,
            )
            .expect("zero-entry push is fine");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("no config entries"), "got: {out:?}");
    }

    #[test]
    fn push_dry_run_emits_url_and_entries_without_posting() {
        let dir = tempdir().expect("tempdir");
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
        ];
        let push_ctx =
            AdapterPushContext::new().with_seed_url("http://127.0.0.1:3000/__edgezero/config/seed");
        let out = SpinCliAdapter
            .push_config_entries(
                dir.path(),
                Some("spin.toml"),
                None,
                &config_store(TEST_CONFIG_ID),
                &entries,
                &push_ctx,
                true,
            )
            .expect("dry-run succeeds");
        assert!(
            out[0].contains("would POST 2 entries to http://127.0.0.1:3000/__edgezero/config/seed"),
            "dry-run header names URL + count: {out:?}"
        );
        assert!(
            out.iter().any(|line| line.contains("`greeting`")),
            "dry-run lists greeting: {out:?}"
        );
        assert!(
            out.iter().any(|line| line.contains("`service.timeout_ms`")),
            "dry-run lists dotted key verbatim (no `.→__`): {out:?}"
        );
    }

    #[test]
    fn push_errors_when_seed_url_unset_prod() {
        let dir = tempdir().expect("tempdir");
        let entries = vec![("greeting".to_owned(), "hi".to_owned())];
        let err = SpinCliAdapter
            .push_config_entries(
                dir.path(),
                Some("spin.toml"),
                None,
                &config_store(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect_err("missing seed URL must error");
        assert!(err.contains("--seed-url"), "names CLI flag: {err}");
        assert!(
            err.contains("EDGEZERO__ADAPTERS__SPIN__SEED_URL"),
            "names prod env var: {err}"
        );
        assert!(
            err.contains("[adapters.spin.commands].seed_url"),
            "names manifest fallback: {err}"
        );
        assert!(
            !err.contains("LOCAL_SEED_URL"),
            "prod chain hint should NOT name the local env var: {err}"
        );
    }

    #[test]
    fn push_errors_when_seed_url_unset_local_names_local_env_var() {
        let dir = tempdir().expect("tempdir");
        let entries = vec![("greeting".to_owned(), "hi".to_owned())];
        let push_ctx = AdapterPushContext::new().with_local(true);
        let err = SpinCliAdapter
            .push_config_entries(
                dir.path(),
                Some("spin.toml"),
                None,
                &config_store(TEST_CONFIG_ID),
                &entries,
                &push_ctx,
                true,
            )
            .expect_err("missing seed URL on local must error");
        assert!(
            err.contains("EDGEZERO__ADAPTERS__SPIN__LOCAL_SEED_URL"),
            "local chain hint names the local env var: {err}"
        );
    }

    #[test]
    fn push_errors_when_seed_token_unset_on_real_push() {
        // Dry-run shouldn't require a token; a real (non-dry-run) push must.
        let dir = tempdir().expect("tempdir");
        let entries = vec![("greeting".to_owned(), "hi".to_owned())];
        let push_ctx = AdapterPushContext::new().with_seed_url("http://localhost:3000/seed");
        let err = SpinCliAdapter
            .push_config_entries(
                dir.path(),
                Some("spin.toml"),
                None,
                &config_store(TEST_CONFIG_ID),
                &entries,
                &push_ctx,
                false,
            )
            .expect_err("missing seed token on real push must error");
        assert!(err.contains("seed token"), "names the missing piece: {err}");
        assert!(
            err.contains("EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN"),
            "names env var: {err}"
        );
        assert!(
            err.contains("NEVER read from edgezero.toml"),
            "documents manifest exclusion for tokens: {err}"
        );
    }

    #[test]
    fn build_seed_payload_emits_d9_body_shape() {
        let payload = build_seed_payload(
            "app_config",
            &[
                ("greeting".to_owned(), "hello".to_owned()),
                ("service.timeout_ms".to_owned(), "1500".to_owned()),
            ],
        );
        assert_eq!(payload["store"].as_str(), Some("app_config"));
        let entries = payload["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["key"].as_str(), Some("greeting"));
        assert_eq!(entries[0]["value"].as_str(), Some("hello"));
        assert_eq!(entries[1]["key"].as_str(), Some("service.timeout_ms"));
        assert_eq!(entries[1]["value"].as_str(), Some("1500"));
    }

    #[test]
    fn build_seed_payload_uses_platform_label_not_logical_id() {
        // T4.7: prove the body carries the platform label so an
        // env-overridden store name flows through correctly.
        let payload =
            build_seed_payload("prod-config", &[("greeting".to_owned(), "hi".to_owned())]);
        assert_eq!(payload["store"].as_str(), Some("prod-config"));
    }
}
