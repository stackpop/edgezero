use std::collections::HashMap;
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

/// Interface implemented by adapter crates to integrate with the `EdgeZero` CLI.
///
/// The non-`execute` methods carry the adapter's `config validate`
/// rules (spec §10). They take primitive parameters (no `Manifest` /
/// `SecretField` from `edgezero-core`) so this crate stays dep-free
/// of `edgezero-core`. Defaults are no-ops; adapters override what
/// they actually need.
pub trait Adapter: Sync + Send {
    /// Execute the requested action with optional adapter-specific args.
    ///
    /// # Errors
    /// Returns an error string if the requested adapter action fails.
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String>;

    /// Name used to reference the adapter (case-insensitive).
    fn name(&self) -> &'static str;

    /// Store kinds for which this adapter is Single-capable per
    /// spec §6.6 — `--strict` rejects `[stores.<kind>].ids.len() > 1`
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
    /// adapter-specific naming constraint — Spin's
    /// `^[a-z][a-z0-9_]*$` after `.→__` translation, for example.
    /// `keys` are the flattened dotted paths into the typed
    /// app-config (e.g. `["greeting", "service.timeout_ms"]`).
    /// Default: no-op.
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
    /// `plain_secrets` carries only `#[secret]` (`KeyInDefault`)
    /// entries as `(field_name, value)`; `#[secret(store_ref)]`
    /// values are runtime store ids and never enter the adapter's
    /// flat variable namespace, so they are excluded by the CLI
    /// before calling. Default: no-op.
    ///
    /// # Errors
    /// Returns a human-readable error string on any conflict
    /// between config keys and secret values (e.g. a Spin variable
    /// collision).
    #[inline]
    fn validate_typed_secrets(
        &self,
        _config_keys: &[&str],
        _plain_secrets: &[(&str, &str)],
    ) -> Result<(), String> {
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
}
