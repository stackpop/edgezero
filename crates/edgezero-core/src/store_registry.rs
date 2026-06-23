//! Per-request store registry — one entry per logical store id.
//!
//! Each adapter builds a [`StoreRegistry`] at request setup, keyed by the
//! logical ids declared in `[stores.<kind>]`. Handlers resolve a handle by id
//! (or via the `_default()` helper for the common single-store case). For
//! adapters that are *Single* for a given store kind (per the
//! capability matrix in the design doc) every id maps to the same
//! flat handle.
//!
//! Type aliases:
//! - [`KvRegistry`] = `StoreRegistry<BoundKvStore>`
//! - [`ConfigRegistry`] = `StoreRegistry<ConfigStoreBinding>`
//! - [`SecretRegistry`] = `StoreRegistry<BoundSecretStore>`
//!
//! KV handles are already bound to a single backing store by construction,
//! so [`BoundKvStore`] is just the existing handle type. Config uses
//! [`ConfigStoreBinding`], which pairs a [`ConfigStoreHandle`] with the
//! default lookup key for that binding (see spec 5.2.1). [`BoundSecretStore`]
//! is a real wrapper because the underlying [`SecretHandle::get_bytes`] takes
//! a `store_name` argument — the registry captures the per-id platform name
//! (resolved from `EDGEZERO__STORES__SECRETS__<ID>__NAME`) so handlers can
//! call [`BoundSecretStore::get_bytes`] with just the key.

use std::collections::BTreeMap;

use bytes::Bytes;

use crate::config_store::ConfigStoreHandle;
use crate::key_value_store::KvHandle;
use crate::secret_store::{SecretError, SecretHandle};

/// A per-bind KV handle, returned by [`KvRegistry::named`] / [`KvRegistry::default`].
pub type BoundKvStore = KvHandle;

/// A per-bind config handle, returned by
/// [`ConfigRegistry::named`] / [`ConfigRegistry::default`].
pub type BoundConfigStore = ConfigStoreHandle;

/// Per-id binding pair for the config store: the handle the
/// extractor calls `get(...)` on, plus the key the extractor
/// looks up by default. The `default_key` is computed by
/// adapters from `EnvConfig::store_key("config", id)`. See spec
/// 5.2.1.
#[derive(Clone, Debug)]
pub struct ConfigStoreBinding {
    /// The default key this binding resolves when no key is specified.
    pub default_key: String,
    /// The config store handle used for key lookups.
    pub handle: ConfigStoreHandle,
}

/// A per-bind secret handle: a [`SecretHandle`] pre-bound to a platform
/// store name. The registry resolves the name per logical id at request
/// setup from `EDGEZERO__STORES__SECRETS__<ID>__NAME` (defaulting to the
/// logical id), so handler code reads
/// `secrets.named(id)?.require_str(key)` without re-passing the platform
/// name on every call.
#[derive(Clone, Debug)]
pub struct BoundSecretStore {
    handle: SecretHandle,
    store_name: String,
}

impl BoundSecretStore {
    /// Retrieve a secret by key against the bound platform store.
    ///
    /// # Errors
    /// See [`SecretHandle::get_bytes`].
    #[inline]
    pub async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, SecretError> {
        self.handle.get_bytes(&self.store_name, key).await
    }

    /// Underlying [`SecretHandle`] (escape hatch for callers that need the
    /// store-name argument explicitly).
    #[inline]
    #[must_use]
    pub fn handle(&self) -> &SecretHandle {
        &self.handle
    }

    /// Bind `handle` to the platform store name `store_name`.
    #[inline]
    #[must_use]
    pub fn new(handle: SecretHandle, store_name: String) -> Self {
        Self { handle, store_name }
    }

    /// Retrieve a secret as raw bytes, error on absent.
    ///
    /// # Errors
    /// See [`SecretHandle::require_bytes`].
    #[inline]
    pub async fn require_bytes(&self, key: &str) -> Result<Bytes, SecretError> {
        self.handle.require_bytes(&self.store_name, key).await
    }

    /// Retrieve a secret as a UTF-8 string, error on absent.
    ///
    /// # Errors
    /// See [`SecretHandle::require_str`].
    #[inline]
    pub async fn require_str(&self, key: &str) -> Result<String, SecretError> {
        self.handle.require_str(&self.store_name, key).await
    }

    /// Platform store name this binding resolves to.
    #[inline]
    #[must_use]
    pub fn store_name(&self) -> &str {
        &self.store_name
    }
}

/// Registry of per-id store handles, with a declared default.
///
/// Constructed by adapters at request setup from the baked store metadata
/// (`Hooks::stores()`) plus the `EDGEZERO__STORES__*` environment overlay.
#[derive(Clone, Debug)]
pub struct StoreRegistry<H: Clone> {
    by_id: BTreeMap<String, H>,
    default_id: String,
}

impl<H: Clone> StoreRegistry<H> {
    /// Return the default handle.
    ///
    /// Always `Some` for a registry constructed via [`Self::new`] — the
    /// invariant is enforced at construction time. `Option` is kept on the
    /// signature for API symmetry with [`Self::named`].
    #[must_use]
    #[inline]
    pub fn default(&self) -> Option<H> {
        self.by_id.get(&self.default_id).cloned()
    }

    /// The resolved default id for this kind.
    #[must_use]
    #[inline]
    pub fn default_id(&self) -> &str {
        &self.default_id
    }

    /// Borrow the default handle without cloning. Mirrors
    /// [`default`](Self::default) but yields a reference.
    #[must_use]
    #[inline]
    pub fn default_ref(&self) -> Option<&H> {
        self.by_id.get(&self.default_id)
    }

    /// Try to build a registry from a pre-built id → handle map and the
    /// declared default id, dropping it entirely when the default id is
    /// not registered. Adapters that skip a failed-to-open backend per id
    /// (logging a warning) call this instead of [`Self::new`] so the
    /// registry isn't constructed with a default that has nowhere to
    /// resolve to. Returning `None` in that case bubbles up as "no
    /// registry wired", which surfaces as a clear 503 at the handler
    /// rather than a silent `None` from [`Self::default`].
    #[must_use]
    #[inline]
    pub fn from_parts(by_id: BTreeMap<String, H>, default_id: String) -> Option<Self> {
        if by_id.is_empty() || !by_id.contains_key(&default_id) {
            return None;
        }
        Some(Self { by_id, default_id })
    }

    /// Iterate over the registered logical ids.
    #[inline]
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.by_id.keys().map(String::as_str)
    }

    /// Look up the handle for `id`. Returns `None` if `id` was not registered.
    #[must_use]
    #[inline]
    pub fn named(&self, id: &str) -> Option<H> {
        self.by_id.get(id).cloned()
    }

    /// Borrow the handle for `id`. Mirrors
    /// [`named`](Self::named) but yields a reference.
    #[must_use]
    #[inline]
    pub fn named_ref(&self, id: &str) -> Option<&H> {
        self.by_id.get(id)
    }

    /// Create a registry from a pre-built id → handle map and the resolved
    /// default id.
    ///
    /// # Panics
    /// Panics (in both debug and release) if `default_id` is not a key in
    /// `by_id`. Adapter builders that drop a failed-to-open id must ensure
    /// they don't construct a registry whose declared default is missing —
    /// either skip the whole registry, or fail the request loudly.
    /// Surfacing this as a panic enforces the [`Self::default`] invariant
    /// at construction time, matching the spec's intent that a declared
    /// default always resolves.
    #[must_use]
    #[inline]
    pub fn new(by_id: BTreeMap<String, H>, default_id: String) -> Self {
        assert!(
            by_id.contains_key(&default_id),
            "StoreRegistry default id `{default_id}` is not present among the registered ids: {ids:?}",
            ids = by_id.keys().collect::<Vec<_>>()
        );
        Self { by_id, default_id }
    }

    /// Build a one-id registry from a single handle, used when an
    /// adapter has a single store and wants to normalise its
    /// wiring to the registry path (so the extractor and
    /// registry-aware accessors don't need a legacy-handle
    /// fallback). `id` is the logical id the handle is registered
    /// under AND the resolved default.
    #[must_use]
    #[inline]
    pub fn single_id(id: String, handle: H) -> Self {
        let mut by_id: BTreeMap<String, H> = BTreeMap::new();
        by_id.insert(id.clone(), handle);
        Self::new(by_id, id)
    }
}

/// Registry of per-id KV handles.
pub type KvRegistry = StoreRegistry<BoundKvStore>;
/// Registry of per-id config bindings (handle + default key).
pub type ConfigRegistry = StoreRegistry<ConfigStoreBinding>;
/// Registry of per-id secret handles.
pub type SecretRegistry = StoreRegistry<BoundSecretStore>;

#[cfg(test)]
mod tests {
    use super::*;

    fn build_registry(entries: &[(&str, &str)], default_id: &str) -> StoreRegistry<String> {
        let by_id: BTreeMap<String, String> = entries
            .iter()
            .map(|(id, value)| ((*id).to_owned(), (*value).to_owned()))
            .collect();
        StoreRegistry::new(by_id, default_id.to_owned())
    }

    fn single_id_registry(id: &'static str, value: &'static str) -> StoreRegistry<&'static str> {
        StoreRegistry::single_id(id.to_owned(), value)
    }

    #[test]
    fn named_returns_handle_for_known_id() {
        let registry = build_registry(&[("sessions", "a"), ("cache", "b")], "sessions");
        assert_eq!(registry.named("cache"), Some("b".to_owned()));
    }

    #[test]
    fn named_returns_none_for_unknown_id() {
        let registry = build_registry(&[("sessions", "a")], "sessions");
        assert_eq!(registry.named("missing"), None);
    }

    #[test]
    fn default_returns_default_handle() {
        let registry = build_registry(&[("sessions", "a"), ("cache", "b")], "cache");
        assert_eq!(registry.default(), Some("b".to_owned()));
    }

    #[test]
    fn default_id_returns_resolved_default() {
        let registry = build_registry(&[("sessions", "a"), ("cache", "b")], "cache");
        assert_eq!(registry.default_id(), "cache");
    }

    #[test]
    fn ids_yields_all_registered_ids_in_sorted_order() {
        let registry = build_registry(&[("cache", "b"), ("sessions", "a")], "sessions");
        let ids: Vec<&str> = registry.ids().collect();
        assert_eq!(ids, vec!["cache", "sessions"]);
    }

    #[test]
    fn registry_is_cloneable() {
        let r1 = build_registry(&[("a", "1")], "a");
        let r2 = r1.clone();
        assert_eq!(r1.named("a"), r2.named("a"));
    }

    #[test]
    #[should_panic(expected = "is not present among the registered ids")]
    fn new_panics_when_default_is_not_among_registered_ids() {
        // The invariant is enforced in both debug and release builds — a
        // builder that drops a failed-to-open default id must not still
        // call `new(by_id, missing_default)`. Catching this loudly avoids
        // silent registries whose `default()` returns `None`.
        let _registry: StoreRegistry<String> = build_registry(&[("sessions", "a")], "cache");
    }

    #[test]
    fn default_ref_and_named_ref_yield_references() {
        let registry = single_id_registry("only", "value");
        assert_eq!(registry.default_ref(), Some(&"value"));
        assert_eq!(registry.named_ref("only"), Some(&"value"));
        assert_eq!(registry.named_ref("missing"), None);
    }
}
