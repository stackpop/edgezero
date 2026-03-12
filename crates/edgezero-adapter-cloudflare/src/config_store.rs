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
use std::sync::{Arc, Mutex, OnceLock};

use edgezero_core::config_store::{ConfigStore, ConfigStoreError};
use worker::Env;

type ConfigMap = HashMap<String, String>;
const CONFIG_CACHE_LIMIT: usize = 64;

/// Config store backed by a single Cloudflare JSON string binding.
///
/// At construction time the binding value is parsed into a `HashMap<String, String>`.
/// Reads are then O(1) map lookups with no further JS interop.
pub struct CloudflareConfigStore {
    data: Arc<ConfigMap>,
}

impl CloudflareConfigStore {
    /// Build a store by reading and parsing the JSON binding named `binding_name`.
    ///
    /// Returns an empty store (graceful fallback) if the binding is absent or
    /// the value is not valid JSON.
    pub fn new(env: &Env, binding_name: &str) -> Self {
        Self::try_new(env, binding_name).unwrap_or_else(Self::empty)
    }

    /// Build a store only when the configured Cloudflare binding exists and parses successfully.
    ///
    /// Missing bindings or invalid JSON are treated as configuration problems, logged at warn
    /// level (once per binding name per isolate lifetime), and return `None` so the adapter
    /// can skip injecting the handle.
    pub fn try_new(env: &Env, binding_name: &str) -> Option<Self> {
        Some(Self {
            data: lookup_cached(env, binding_name)?,
        })
    }

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
}

impl ConfigStore for CloudflareConfigStore {
    fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        Ok(self.data.get(key).cloned())
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
        .unwrap_or_else(|p| p.into_inner())
        .get(binding_name)
    {
        return entry;
    }

    // Cache miss: resolve from the JS env (synchronous interop, safe outside the lock).
    let resolved = match env.var(binding_name).ok().map(|v| v.to_string()) {
        None => {
            log::warn!(
                "configured config store binding '{}' is missing from the Worker environment; skipping config-store injection",
                binding_name
            );
            None
        }
        Some(raw) => match serde_json::from_str::<ConfigMap>(&raw) {
            Ok(data) => Some(Arc::new(data)),
            Err(err) => {
                log::warn!(
                    "configured config store binding '{}' contains invalid JSON: {}; skipping config-store injection",
                    binding_name,
                    err
                );
                None
            }
        },
    };

    config_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(binding_name, resolved, CONFIG_CACHE_LIMIT)
}

fn config_cache() -> &'static Mutex<ConfigCache> {
    static CACHE: OnceLock<Mutex<ConfigCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ConfigCache::default()))
}

#[derive(Default)]
struct ConfigCache {
    entries: HashMap<String, Option<Arc<ConfigMap>>>,
    order: VecDeque<String>,
}

impl ConfigCache {
    fn get(&self, key: &str) -> Option<Option<Arc<ConfigMap>>> {
        self.entries.get(key).cloned()
    }

    fn insert(
        &mut self,
        key: &str,
        value: Option<Arc<ConfigMap>>,
        limit: usize,
    ) -> Option<Arc<ConfigMap>> {
        if let Some(existing) = self.entries.get(key) {
            return existing.clone();
        }

        if limit > 0 && self.order.len() >= limit {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }

        let key = key.to_string();
        self.order.push_back(key.clone());
        self.entries.insert(key, value.clone());
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    edgezero_core::config_store_contract_tests!(cloudflare_config_store_contract, #[wasm_bindgen_test], {
        CloudflareConfigStore::from_entries([
            ("contract.key.a".to_string(), "value_a".to_string()),
            ("contract.key.b".to_string(), "value_b".to_string()),
        ])
    });
}
