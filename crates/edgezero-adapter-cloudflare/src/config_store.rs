//! Cloudflare Workers adapter config store: reads a single JSON env var.
//!
//! Config is stored as one Cloudflare string binding (set in `wrangler.toml [vars]`)
//! whose value is a JSON object, e.g.:
//!
//! ```toml
//! [vars]
//! app_config = '{"greeting":"hello","feature.new_checkout":"false"}'
//! ```
//!
//! This allows arbitrary string keys (including dots) on a platform whose binding
//! names are restricted to JavaScript identifier syntax.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use edgezero_core::config_store::{ConfigStore, ConfigStoreError};
use worker::Env;

/// Maximum number of distinct binding names to remember in the parse cache.
///
/// A single Worker typically uses one or two config bindings; 64 is a generous
/// ceiling that bounds isolate memory without any practical limit for real apps.
/// When the cache is full, the oldest entry is evicted (LRU-style) to make room.
const CONFIG_CACHE_LIMIT: usize = 64;

type ConfigMap = HashMap<String, String>;

#[derive(Clone)]
enum CacheEntry {
    Missing,
    Present(Arc<ConfigMap>),
}

#[derive(Default)]
struct ConfigCache {
    entries: HashMap<String, CacheEntry>,
    order: VecDeque<String>,
}

/// Config store backed by a single Cloudflare JSON string binding.
///
/// At construction time the binding value is parsed into a `HashMap<String, String>`.
/// Reads are then O(1) map lookups with no further JS interop.
pub struct CloudflareConfigStore {
    data: Arc<ConfigMap>,
}

impl ConfigCache {
    fn get(&self, key: &str) -> Option<CacheEntry> {
        self.entries.get(key).cloned()
    }

    fn get_or_insert(
        &mut self,
        key: &str,
        entry: CacheEntry,
        limit: usize,
    ) -> Option<Arc<ConfigMap>> {
        if let Some(existing) = self.entries.get(key) {
            return entry_to_value(existing);
        }

        if limit > 0 && self.order.len() >= limit {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }

        let owned_key = key.to_owned();
        self.order.push_back(owned_key.clone());
        let resolved = entry_to_value(&entry);
        self.entries.insert(owned_key, entry);
        resolved
    }
}

impl CloudflareConfigStore {
    fn empty() -> Self {
        Self {
            data: Arc::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            data: Arc::new(entries.into_iter().collect()),
        }
    }

    /// Build a store by reading and parsing the JSON binding named `binding_name`.
    ///
    /// Returns an empty store (every key returns `None`) if the binding is absent or
    /// its value is not valid JSON. Missing or invalid bindings are logged at `warn`
    /// level (once per binding name per isolate lifetime) via the same path as
    /// [`Self::try_new`], so misconfigured binding names will surface in logs.
    /// Use [`Self::try_new`] when you need to distinguish a missing/invalid binding
    /// from a valid but empty config at the call site.
    #[inline]
    pub fn new_or_empty(env: &Env, binding_name: &str) -> Self {
        Self::try_new(env, binding_name).unwrap_or_else(Self::empty)
    }

    /// Build a store only when the configured Cloudflare binding exists and parses successfully.
    ///
    /// Missing bindings or invalid JSON are treated as configuration problems, logged at warn
    /// level (once per binding name per isolate lifetime), and return `None` so the adapter
    /// can skip injecting the handle.
    #[inline]
    #[must_use]
    pub fn try_new(env: &Env, binding_name: &str) -> Option<Self> {
        Some(Self {
            data: lookup_cached(env, binding_name)?,
        })
    }
}

impl ConfigStore for CloudflareConfigStore {
    #[inline]
    fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        Ok(self.data.get(key).cloned())
    }
}

fn config_cache() -> &'static Mutex<ConfigCache> {
    static CACHE: OnceLock<Mutex<ConfigCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ConfigCache::default()))
}

fn entry_to_value(entry: &CacheEntry) -> Option<Arc<ConfigMap>> {
    match entry {
        CacheEntry::Missing => None,
        CacheEntry::Present(arc) => Some(Arc::clone(arc)),
    }
}

/// Parse-and-cache the config map for `binding_name`.
///
/// Keyed only by name: Cloudflare env vars are immutable within an isolate
/// lifetime, so the parsed result for a given binding name never changes.
/// Warnings are suppressed for recently seen binding names via a bounded cache.
///
/// # WASM safety
/// `std::sync::Mutex` compiles for `wasm32-unknown-unknown` and is safe here because
/// WASM is single-threaded — the lock can never be contested and poisoning cannot
/// occur via a concurrent thread panic.
fn lookup_cached(env: &Env, binding_name: &str) -> Option<Arc<ConfigMap>> {
    // Fast path: already cached.
    if let Some(entry) = config_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(binding_name)
    {
        return entry_to_value(&entry);
    }

    // Cache miss: resolve from the JS env (synchronous interop, safe outside the lock).
    let resolved = match env.var(binding_name).ok().map(|value| value.to_string()) {
        None => {
            log::warn!(
                "configured config store binding '{binding_name}' is missing from the Worker environment; skipping config-store injection"
            );
            CacheEntry::Missing
        }
        Some(raw) => match serde_json::from_str::<ConfigMap>(&raw) {
            Ok(data) => CacheEntry::Present(Arc::new(data)),
            Err(err) => {
                log::warn!(
                    "configured config store binding '{binding_name}' contains invalid JSON: {err}; skipping config-store injection"
                );
                CacheEntry::Missing
            }
        },
    };

    // Cache the resolved value — including Missing for absent/invalid bindings.
    // This is safe because Cloudflare string bindings are immutable within an
    // isolate lifetime: the parsed result for a given binding name never changes,
    // so caching a failed parse prevents redundant warnings on every request.
    config_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert(binding_name, resolved, CONFIG_CACHE_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    edgezero_core::config_store_contract_tests!(cloudflare_config_store_contract, #[wasm_bindgen_test], {
        CloudflareConfigStore::from_entries([
            ("contract.key.a".to_owned(), "value_a".to_owned()),
            ("contract.key.b".to_owned(), "value_b".to_owned()),
        ])
    });
}
