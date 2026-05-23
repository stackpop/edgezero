//! Per-request store registry — one entry per logical store id.
//!
//! Each adapter builds a [`StoreRegistry`] at request setup, keyed by the
//! logical ids declared in `[stores.<kind>]`. Handlers resolve a handle by id
//! (or via the `_default()` helper for the common single-store case). For
//! adapters that are *Single* for a given kind (§6.6 capability matrix) every
//! id maps to the same flat handle.
//!
//! Type aliases:
//! - [`KvRegistry`] = `StoreRegistry<BoundKvStore>`
//! - [`ConfigRegistry`] = `StoreRegistry<BoundConfigStore>`
//! - [`SecretRegistry`] = `StoreRegistry<BoundSecretStore>`
//!
//! The `Bound*` aliases are the per-id resolved handles — currently identical
//! to [`crate::key_value_store::KvHandle`] /
//! [`crate::config_store::ConfigStoreHandle`] /
//! [`crate::secret_store::SecretHandle`]. They exist so handler code and
//! extractors can be expressed in registry-aware terms without coupling to
//! the legacy single-handle names.

use std::collections::BTreeMap;

use crate::config_store::ConfigStoreHandle;
use crate::key_value_store::KvHandle;
use crate::secret_store::SecretHandle;

/// A per-bind KV handle, returned by [`KvRegistry::named`] / [`KvRegistry::default`].
pub type BoundKvStore = KvHandle;

/// A per-bind config handle, returned by
/// [`ConfigRegistry::named`] / [`ConfigRegistry::default`].
pub type BoundConfigStore = ConfigStoreHandle;

/// A per-bind secret handle, returned by
/// [`SecretRegistry::named`] / [`SecretRegistry::default`].
pub type BoundSecretStore = SecretHandle;

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
    /// Return the default handle, if the registry is non-empty.
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

    /// Create a registry from a pre-built id → handle map and the resolved
    /// default id. The default id must be present in `by_id`.
    #[must_use]
    #[inline]
    pub fn new(by_id: BTreeMap<String, H>, default_id: String) -> Self {
        debug_assert!(
            by_id.contains_key(&default_id),
            "StoreRegistry default id `{default_id}` is not present in the map"
        );
        Self { by_id, default_id }
    }
}

/// Registry of per-id KV handles.
pub type KvRegistry = StoreRegistry<BoundKvStore>;
/// Registry of per-id config handles.
pub type ConfigRegistry = StoreRegistry<BoundConfigStore>;
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
}
