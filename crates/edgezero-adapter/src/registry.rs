use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{LazyLock, PoisonError, RwLock};

static REGISTRY: LazyLock<RwLock<HashMap<String, &'static dyn Adapter>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Actions the `EdgeZero` CLI can request from an adapter implementation.
///
/// `AuthLogin` / `AuthLogout` / `AuthStatus` dispatch the platform's
/// native sign-in flow (`wrangler login`, `fastly profile create`,
/// `spin cloud login`, …). The adapter chooses whether to shell out
/// to a CLI, call an HTTP API, or no-op — the CLI doesn't care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdapterAction {
    AuthLogin,
    AuthLogout,
    AuthStatus,
    Build,
    Deploy,
    Serve,
}

/// Provision dispatch mode. `Cloud` keeps today's cloud-CLI shell-out
/// behaviour; `Local` writes adapter-local emulator state (no cloud
/// calls). Threaded through `Adapter::provision` so each adapter
/// branches once at the top of its impl. See spec §"CLI / trait
/// surface".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionMode {
    Cloud,
    Local,
}

/// Adapter-emitted deployed identifiers. Kept neutral (string-keyed
/// maps only) so `edgezero-adapter` stays dep-free of
/// `edgezero-core` -- the CLI maps this into the strongly typed
/// `ManifestAdapterDeployed` shape when writing `edgezero.toml`.
/// See spec §"Writeback ownership".
#[derive(Debug, Default, Clone)]
pub struct AdapterDeployedState {
    pub fields: BTreeMap<String, String>,
    pub sub_tables: BTreeMap<String, BTreeMap<String, String>>,
}

/// Return value of `Adapter::provision` (and `provision_typed`).
/// `status_lines` are operator-facing; `deployed`, when `Some`,
/// records the cloud-returned identifiers the CLI persists into
/// `edgezero.toml`'s `[adapters.<name>.deployed]` block. Local
/// provision returns `deployed: None`.
#[derive(Debug, Default, Clone)]
pub struct ProvisionOutcome {
    pub deployed: Option<AdapterDeployedState>,
    pub status_lines: Vec<String>,
}

/// A single declared store id, paired with the platform name the
/// runtime will resolve via `EDGEZERO__STORES__<KIND>__<ID>__NAME`.
///
/// The CLI's `provision` and `push` paths resolve the env override
/// once (against `std::env`) and pass both names through, so the
/// adapter writes the PLATFORM name into wrangler.toml /
/// spin.toml / fastly.toml. Without the platform name on this
/// side, `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config`
/// would be silently ignored at provision time and the runtime
/// would later look up a binding named `prod_config` that
/// provision never created.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedStoreId {
    /// The logical id declared in `[stores.<kind>].ids`. Used for
    /// human-facing messages and for the validate/strict checks.
    pub logical: String,
    /// The platform name the runtime resolves at request time --
    /// `EDGEZERO__STORES__<KIND>__<LOGICAL>__NAME` or, when unset,
    /// the logical id itself.
    pub platform: String,
}

impl ResolvedStoreId {
    /// Shorthand for the common case where the platform name
    /// equals the logical id (no env override applied).
    #[must_use]
    #[inline]
    pub fn from_logical<S: Into<String>>(logical: S) -> Self {
        let logical_str = logical.into();
        Self {
            platform: logical_str.clone(),
            logical: logical_str,
        }
    }

    /// Test helper: collect a slice of logical ids into a
    /// `Vec<ResolvedStoreId>` with platform names defaulted to the
    /// logical ids themselves (no env overlay). Keeps the
    /// per-adapter test fixtures terse.
    #[must_use]
    #[inline]
    pub fn from_logicals(logicals: &[&str]) -> Vec<Self> {
        logicals.iter().copied().map(Self::from_logical).collect()
    }

    /// Construct a resolved id with explicit logical and platform
    /// names. Useful for tests that exercise the env-overlay
    /// case + for the CLI's manual `resolve_kind` helper.
    #[must_use]
    #[inline]
    pub fn new<L: Into<String>, P: Into<String>>(logical: L, platform: P) -> Self {
        Self {
            logical: logical.into(),
            platform: platform.into(),
        }
    }
}

/// Per-kind store ids extracted from `[stores.<kind>].ids` in the
/// manifest, with each id paired against its env-resolved platform
/// name (`EDGEZERO__STORES__<KIND>__<ID>__NAME` or the id itself).
/// Handed to [`Adapter::provision`] so the adapter writes the
/// PLATFORM name into the per-platform manifest -- not the
/// logical id, which the runtime would never look up.
///
/// Empty slices mean the user didn't declare that store kind.
#[derive(Clone, Copy, Debug)]
pub struct ProvisionStores<'stores> {
    pub config: &'stores [ResolvedStoreId],
    pub kv: &'stores [ResolvedStoreId],
    pub secrets: &'stores [ResolvedStoreId],
}

/// Context passed to [`Adapter::push_config_entries`] and
/// [`Adapter::push_config_entries_local`] carrying already-resolved
/// `config push` overlay values.
///
/// The CLI's `dispatch_push` builds this via the builder API
/// ([`Self::new`] + the `with_*` setters) so future fields can be
/// added without breaking out-of-tree adapters that just RECEIVE
/// it via the trait method. `#[non_exhaustive]` enforces that
/// downstream construction stays inside the builder.
///
/// Lifetime: borrows the resolved strings from the CLI's owned
/// `PushContext` (config.rs) so adapters see `Option<&_>` without
/// any extra cloning.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct AdapterPushContext<'ctx> {
    /// `true` when the operator passed `--local`. Adapters that
    /// have a separate local-emulator path use this to pick the
    /// right writeback target; adapters where local == default
    /// can ignore it.
    pub local: bool,
    /// `[adapters.<name>.commands].deploy` from the manifest, if set.
    /// Adapters use this to auto-detect the deployment target —
    /// e.g. Spin treats `spin deploy` / `spin cloud deploy` as a
    /// signal to shell out to `spin cloud key-value set` instead of
    /// writing local `SQLite`. `None` means the operator left the
    /// deploy command unset (or no manifest entry exists for this
    /// adapter), in which case auto-detection silently does not
    /// fire.
    pub manifest_adapter_deploy_cmd: Option<&'ctx str>,
    /// Already-resolved path to the adapter's runtime configuration
    /// file (e.g. Spin's `runtime-config.toml`, which declares the
    /// `[key_value_store.<label>]` backends `config push --adapter
    /// spin` dispatches into). `None` means the operator did not
    /// pass `--runtime-config`; the adapter resolves a default
    /// location (typically next to the adapter manifest).
    pub runtime_config_path: Option<&'ctx Path>,
}

impl<'ctx> AdapterPushContext<'ctx> {
    /// Construct a default context: no runtime-config path, prod
    /// (not local). Rust rejects struct-literal construction of
    /// `#[non_exhaustive]` types from outside the defining crate, so
    /// the CLI MUST build via this constructor and the `with_*`
    /// setters below.
    #[must_use]
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the `--local` flag.
    #[must_use]
    #[inline]
    pub fn with_local(mut self, local: bool) -> Self {
        self.local = local;
        self
    }

    /// Set the manifest-adapter deploy command.
    #[must_use]
    #[inline]
    pub fn with_manifest_adapter_deploy_cmd(mut self, cmd: &'ctx str) -> Self {
        self.manifest_adapter_deploy_cmd = Some(cmd);
        self
    }

    /// Set the runtime-config path.
    #[must_use]
    #[inline]
    pub fn with_runtime_config_path(mut self, path: &'ctx Path) -> Self {
        self.runtime_config_path = Some(path);
        self
    }
}

/// Per-secret-key entry passed to
/// [`Adapter::validate_typed_secrets`]. `#[non_exhaustive]` for
/// v2 source-compat; construction goes through `new`.
#[non_exhaustive]
pub struct TypedSecretEntry<'entry> {
    /// Rust struct field name (e.g. `"api_token"`).
    pub field_name: &'entry str,
    /// Blob value — i.e. the secret-store KEY NAME.
    pub key_value: &'entry str,
    /// Logical secret-store id this key targets.
    pub store_id: &'entry str,
}

impl<'entry> TypedSecretEntry<'entry> {
    /// Construct a new entry from its three components.
    #[must_use]
    #[inline]
    pub fn new(store_id: &'entry str, field_name: &'entry str, key_value: &'entry str) -> Self {
        Self {
            field_name,
            key_value,
            store_id,
        }
    }
}

/// Outcome of a single-key read. See spec 9.0.
#[non_exhaustive]
pub enum ReadConfigEntry {
    /// The store exists but the key is absent (operator hasn't pushed yet,
    /// or pushed under a different key).
    MissingKey,
    /// The store itself is absent — wrangler.toml has no matching binding,
    /// fastly.toml has no setup table, axum's local-config-<id>.json file
    /// doesn't exist yet.
    MissingStore,
    /// The remote held the key; the body is the serialised envelope JSON.
    Present(String),
    /// The adapter cannot query the backend for this entry — e.g. Spin
    /// Cloud's CLI exposes no `get`. `&'static str` carries the human-
    /// readable reason. See spec 8.3 four-branch UX.
    Unsupported(&'static str),
}

/// Interface implemented by adapter crates to integrate with the `EdgeZero` CLI.
///
/// The non-`execute` methods carry the adapter's `config validate`
/// rules. They take primitive parameters (no `Manifest` /
/// `SecretField` from `edgezero-core`) so this crate stays dep-free
/// of `edgezero-core`. Defaults are no-ops; adapters override what
/// they actually need.
pub trait Adapter: Sync + Send {
    /// Execute the requested action with optional adapter-specific args.
    ///
    /// `args` is a stringly-typed pass-through for arguments meant
    /// for the underlying native CLI (`wrangler` / `fastly` / `spin`):
    /// `edgezero build --adapter cloudflare -- --foo bar` forwards
    /// `["--foo", "bar"]` here. The loose typing is deliberate for
    /// passthrough but stands out against the typed `provision` /
    /// `push_config_entries` parameters below. A future cleanup
    /// could replace the enum + string-vec pair with per-action
    /// typed parameter structs (e.g. `BuildArgs { manifest_root,
    /// extra_args }`) mirroring the rest of the trait.
    ///
    /// # Errors
    /// Returns an error string if the requested adapter action fails.
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String>;

    /// Store kinds whose logical-id namespaces the adapter merges into
    /// a single backend at runtime — declaring the SAME logical id
    /// under two merged kinds causes silent write collisions because
    /// `provision` resolves both to the same platform label, and
    /// runtime writes from `kv_store("x")` and `config_store("x")`
    /// hit the same underlying store. `config validate` rejects such
    /// overlap. Default: `&[]` (kinds are independent for all
    /// backends).
    ///
    /// Spin overrides this to `&["kv", "config"]` because both kinds
    /// back to `spin_sdk::key_value::Store` via the same `provision`
    /// path that writes labels into `[component.<id>].key_value_stores`.
    #[inline]
    fn merged_id_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    /// Name used to reference the adapter (case-insensitive).
    fn name(&self) -> &'static str;

    /// Provision the platform resources backing each store id the
    /// user declared. Returns a list of human-readable
    /// status lines the CLI logs verbatim — one line per resource
    /// created, skipped, or that would be created under `dry_run`.
    ///
    /// `manifest_root` is the directory containing the user's
    /// `edgezero.toml`. `adapter_manifest_path` and
    /// `component_selector` come from `[adapters.<name>.adapter]`
    /// — the adapter resolves its own per-platform manifest
    /// (`wrangler.toml`, `fastly.toml`, `spin.toml`) relative to
    /// the root. `stores` carries the declared ids per kind.
    ///
    /// `deployed` carries the adapter's previously-persisted
    /// deployed identifiers (e.g. Cloudflare KV namespace ids,
    /// Fastly service id). Local-arm impls consult it for
    /// precedence rules (spec §"CLI / trait surface"); cloud-arm
    /// impls pass `None` — they produce, not consume, the deployed
    /// state. `mode` selects cloud vs. local emulator paths
    /// (spec §"CLI / trait surface", §"Writeback ownership").
    ///
    /// No default impl is provided — every adapter must update
    /// explicitly so the compiler flags any missed call sites.
    ///
    /// # Errors
    /// Returns a human-readable error string if any platform
    /// invocation or manifest edit fails. `dry_run` impls should
    /// describe what they *would* do without performing it.
    #[expect(
        clippy::too_many_arguments,
        reason = "provision needs the manifest root, adapter manifest path, component selector, resolved stores, previously-deployed state (for local-arm precedence), dispatch mode (cloud vs local), and dry-run flag — 8 args. Each is distinct; an aggregate struct would be a larger ergonomic regression for adapter implementers."
    )]
    fn provision(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        stores: &ProvisionStores<'_>,
        deployed: Option<&AdapterDeployedState>,
        mode: ProvisionMode,
        dry_run: bool,
    ) -> Result<ProvisionOutcome, String>;

    /// Push config entries into the platform's config store backing
    /// `store_id`. Returns a list of human-readable status lines the
    /// CLI logs verbatim.
    ///
    /// Since the blob app-config cutover, `entries` typically carries
    /// **one entry per push**: `(logical_key, blob_envelope_json)`.
    /// The value is an opaque JSON string (the serialised
    /// `BlobEnvelope`) — the adapter writes it as-is without
    /// per-leaf flattening. No secret stripping or dotted-key
    /// expansion happens here; the envelope is a single atomic blob.
    ///
    /// `manifest_root`, `adapter_manifest_path`, and
    /// `component_selector` mirror `provision` — each adapter
    /// resolves its own per-platform manifest as needed.
    ///
    /// Default: returns an error. Adapters opt in by overriding.
    ///
    /// # Errors
    /// Returns a human-readable error string if the platform
    /// invocation or manifest edit fails, or the adapter has no
    /// `push` impl. `dry_run` impls describe what they *would* do
    /// without performing it.
    #[inline]
    #[expect(
        clippy::too_many_arguments,
        reason = "config push needs the manifest root, adapter manifest path, component selector, resolved store, entries, push-time overlay (AdapterPushContext), and dry-run flag — 8 args. Each is distinct and the alternative aggregate struct is a bigger ergonomic regression for adapter implementers than the lint cost."
    )]
    fn push_config_entries(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        _store: &ResolvedStoreId,
        _entries: &[(String, String)],
        _push_ctx: &AdapterPushContext<'_>,
        _dry_run: bool,
    ) -> Result<Vec<String>, String> {
        Err(format!(
            "adapter `{}` does not implement `config push`",
            self.name()
        ))
    }

    /// Push config entries into the adapter's **local emulator** state
    /// instead of the live platform — `config push --local`. Used when
    /// developing against a local runtime (Viceroy for Fastly,
    /// `wrangler dev --local` for Cloudflare) where the production
    /// platform CLI doesn't help.
    ///
    /// Entry shape mirrors [`Self::push_config_entries`]: typically one
    /// `(logical_key, blob_envelope_json)` tuple written as-is.
    ///
    /// Arguments + return shape mirror [`Self::push_config_entries`].
    ///
    /// Default: returns an error. Adapters opt in by overriding.
    /// Adapters whose production push is already local-only (axum
    /// writes a JSON file under `.edgezero/`; spin edits `spin.toml`)
    /// should override to delegate to [`Self::push_config_entries`].
    ///
    /// # Errors
    /// Returns a human-readable error string if the local-state edit
    /// fails or the adapter has no `--local` impl. `dry_run` impls
    /// describe what they *would* do without performing it.
    #[inline]
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors `push_config_entries` — same 8-argument shape."
    )]
    fn push_config_entries_local(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        _store: &ResolvedStoreId,
        _entries: &[(String, String)],
        _push_ctx: &AdapterPushContext<'_>,
        _dry_run: bool,
    ) -> Result<Vec<String>, String> {
        Err(format!(
            "adapter `{}` does not implement `config push --local`",
            self.name()
        ))
    }

    /// Single-key read against the LIVE platform. Mirrors
    /// [`Self::push_config_entries`]'s argument list per spec 9.0 so
    /// adapters can share helpers (`find_namespace_id` for Cloudflare,
    /// `resolve_label_for_store` for Spin, etc.).
    ///
    /// Default: returns [`ReadConfigEntry::Unsupported`]. Adapters opt
    /// in by overriding.
    ///
    /// # Errors
    /// Returns a human-readable error string if the platform invocation
    /// itself fails (network error, malformed response, etc.). A missing
    /// key or store is NOT an error — use the appropriate enum variant.
    #[inline]
    fn read_config_entry(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        _store: &ResolvedStoreId,
        _key: &str,
        _push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String> {
        Ok(ReadConfigEntry::Unsupported(
            "adapter does not implement remote read-back",
        ))
    }

    /// Single-key read against the LOCAL emulator state. Mirrors
    /// [`Self::push_config_entries_local`].
    ///
    /// Default: returns [`ReadConfigEntry::Unsupported`]. Adapters opt
    /// in by overriding.
    ///
    /// # Errors
    /// Returns a human-readable error string if the local-state read
    /// itself fails. A missing key or store is NOT an error — use the
    /// appropriate enum variant.
    #[inline]
    fn read_config_entry_local(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        _store: &ResolvedStoreId,
        _key: &str,
        _push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String> {
        Ok(ReadConfigEntry::Unsupported(
            "adapter does not implement local read-back",
        ))
    }

    /// Store kinds for which this adapter is Single-capable per
    /// spec — `--strict` rejects `[stores.<kind>].ids.len() > 1`
    /// when any listed kind matches. Default: `&[]` (Multi for
    /// every store kind).
    #[inline]
    fn single_store_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    /// Adapter-specific manifest check — e.g. Spin's
    /// `[component.*]` discovery in `spin.toml`. The adapter
    /// resolves its own per-adapter manifest path relative to
    /// `manifest_root` (the directory containing the user's
    /// `edgezero.toml`). `adapter_manifest_path` and
    /// `component_selector` come from
    /// `[adapters.<name>.adapter].manifest` and `.component`
    /// respectively. Default: no-op.
    ///
    /// # Errors
    /// Returns a human-readable error string on any manifest
    /// inconsistency the adapter can detect.
    #[inline]
    fn validate_adapter_manifest(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Reject the user's `<name>.toml` if it violates an
    /// adapter-specific naming constraint on raw config keys.
    /// `keys` are the flattened dotted paths into the typed
    /// app-config (e.g. `["greeting", "service.timeout_ms"]`).
    /// No registered adapter currently overrides this — Spin's
    /// previous `^[a-z][a-z0-9_]*$` rule lapsed when config moved
    /// to KV — but the hook stays for future adapters whose
    /// stores impose a naming convention. Default: no-op.
    ///
    /// # Errors
    /// Returns a human-readable error string if any key violates
    /// the adapter's contract.
    #[inline]
    fn validate_app_config_keys(&self, _keys: &[&str]) -> Result<(), String> {
        Ok(())
    }

    /// Typed-only check that needs `#[secret]` field values — the
    /// CLI calls this only from the typed validation flow.
    /// `entries` carries both `KeyInDefault` and `KeyInNamedStore`
    /// entries as [`TypedSecretEntry`] values; `StoreRef` values are
    /// runtime store ids and never enter the adapter's flat variable
    /// namespace, so they are excluded by the CLI before calling.
    /// Default: no-op.
    ///
    /// Note: the previous signature took a `_config_keys` parameter
    /// so Spin could detect cross-namespace collision with KV-stored
    /// values; KV-backed config dropped that need in Stage 6, and no
    /// remaining adapter consults it. If a future adapter needs the
    /// flattened config-key set here, add it back via a builder
    /// context rather than re-introducing a positional parameter
    /// every adapter has to ignore.
    ///
    /// # Errors
    /// Returns a human-readable error string on any adapter-
    /// specific conflict — e.g. two `#[secret]` values that
    /// collapse to the same Spin variable name under the
    /// runtime's canonicalisation.
    #[inline]
    fn validate_typed_secrets(&self, _entries: &[TypedSecretEntry<'_>]) -> Result<(), String> {
        Ok(())
    }
}

/// Registers an adapter so it can be discovered by the CLI.
#[inline]
pub fn register_adapter(adapter: &'static dyn Adapter) {
    let mut registry = REGISTRY.write().unwrap_or_else(PoisonError::into_inner);
    registry.insert(adapter.name().to_ascii_lowercase(), adapter);
}

/// Looks up an adapter by name.
#[inline]
pub fn get_adapter(name: &str) -> Option<&'static dyn Adapter> {
    let registry = REGISTRY.read().unwrap_or_else(PoisonError::into_inner);
    registry.get(&name.to_ascii_lowercase()).copied()
}

/// Returns the names of all registered adapters.
#[inline]
pub fn registered_adapters() -> Vec<String> {
    let registry = REGISTRY.read().unwrap_or_else(PoisonError::into_inner);
    let mut names: Vec<String> = registry.keys().cloned().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{LazyLock, Mutex};

    static FIRST: TestAdapter = TestAdapter {
        hit_value: 1,
        name: "dummy",
    };
    static HIT: AtomicUsize = AtomicUsize::new(0);
    static OTHER: TestAdapter = TestAdapter {
        hit_value: 3,
        name: "other",
    };
    static SECOND: TestAdapter = TestAdapter {
        hit_value: 2,
        name: "dummy",
    };
    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct TestAdapter {
        hit_value: usize,
        name: &'static str,
    }

    #[expect(
        clippy::missing_trait_methods,
        reason = "TestAdapter only exercises register / get / execute; the validation methods inherit the trait defaults (no-ops)"
    )]
    impl Adapter for TestAdapter {
        fn execute(&self, _action: AdapterAction, _args: &[String]) -> Result<(), String> {
            HIT.store(self.hit_value, Ordering::SeqCst);
            Ok(())
        }

        fn name(&self) -> &'static str {
            self.name
        }

        fn provision(
            &self,
            _manifest_root: &Path,
            _adapter_manifest_path: Option<&str>,
            _component_selector: Option<&str>,
            _stores: &ProvisionStores<'_>,
            _deployed: Option<&AdapterDeployedState>,
            _mode: ProvisionMode,
            _dry_run: bool,
        ) -> Result<ProvisionOutcome, String> {
            Ok(ProvisionOutcome::default())
        }
    }

    fn reset() {
        let mut registry = super::REGISTRY.write().expect("registry lock");
        registry.clear();
        HIT.store(0, Ordering::SeqCst);
    }

    #[test]
    fn registers_and_fetches_adapter() {
        let _guard = TEST_LOCK.lock().expect("lock");
        reset();
        register_adapter(&FIRST);
        let adapter = get_adapter("dummy").expect("adapter present");
        adapter
            .execute(AdapterAction::Build, &[])
            .expect("execute succeeds");
        assert_eq!(HIT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn latest_registration_overrides_previous() {
        let _guard = TEST_LOCK.lock().expect("lock");
        reset();
        register_adapter(&FIRST);
        register_adapter(&SECOND);
        let adapter = get_adapter("dummy").expect("adapter present");
        adapter
            .execute(AdapterAction::Deploy, &[])
            .expect("execute succeeds");
        assert_eq!(HIT.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn registered_adapters_are_sorted() {
        let _guard = TEST_LOCK.lock().expect("lock");
        reset();
        register_adapter(&OTHER);
        register_adapter(&FIRST);
        let adapters = registered_adapters();
        assert_eq!(adapters, vec!["dummy".to_owned(), "other".to_owned()]);
    }

    #[test]
    fn default_read_config_entry_returns_unsupported() {
        let root = Path::new("/tmp");
        let store = ResolvedStoreId::from_logical("app_config");
        let ctx = AdapterPushContext::new();
        let result = FIRST
            .read_config_entry(root, None, None, &store, "greeting", &ctx)
            .expect("default impl is infallible");
        assert!(
            matches!(result, ReadConfigEntry::Unsupported(_)),
            "expected Unsupported variant from default impl"
        );
        let local_result = FIRST
            .read_config_entry_local(root, None, None, &store, "greeting", &ctx)
            .expect("default local impl is infallible");
        assert!(
            matches!(local_result, ReadConfigEntry::Unsupported(_)),
            "expected Unsupported variant from default local impl"
        );
    }

    #[test]
    fn provision_outcome_default_is_empty() {
        let outcome = ProvisionOutcome::default();
        assert!(outcome.status_lines.is_empty());
        assert!(outcome.deployed.is_none());
    }

    #[test]
    fn adapter_deployed_state_round_trips_via_btreemap() {
        use std::collections::BTreeMap;
        let mut state = AdapterDeployedState::default();
        state.fields.insert("service_id".into(), "SVC1".into());
        let mut kv = BTreeMap::new();
        kv.insert("sessions".into(), "abc123".into());
        state.sub_tables.insert("kv_namespaces".into(), kv);
        assert_eq!(state.fields["service_id"], "SVC1");
        assert_eq!(state.sub_tables["kv_namespaces"]["sessions"], "abc123");
    }
}
