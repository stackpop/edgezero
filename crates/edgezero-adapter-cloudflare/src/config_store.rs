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

use std::collections::HashMap;

use edgezero_core::config_store::ConfigStore;
use worker::Env;

/// Config store backed by a single Cloudflare JSON string binding.
///
/// At construction time the binding value is parsed into a `HashMap<String, String>`.
/// Reads are then O(1) map lookups with no further JS interop.
pub struct CloudflareConfigStore {
    data: HashMap<String, String>,
}

impl CloudflareConfigStore {
    /// Build a store by reading and parsing the JSON binding named `binding_name`.
    ///
    /// Returns an empty store (graceful fallback) if the binding is absent or
    /// the value is not valid JSON.
    pub fn new(env: &Env, binding_name: &str) -> Self {
        let raw = env.var(binding_name).ok();
        if raw.is_none() {
            log::info!(
                "config store binding '{}' is not set in wrangler.toml [vars]; proceeding without config",
                binding_name
            );
        }
        let data = raw
            .and_then(|v| {
                let s = v.to_string();
                serde_json::from_str(&s)
                    .map_err(|e| {
                        log::warn!(
                            "config store binding '{}' is not valid JSON: {}; proceeding without config",
                            binding_name,
                            e
                        );
                        e
                    })
                    .ok()
            })
            .unwrap_or_default();
        Self { data }
    }
}

impl ConfigStore for CloudflareConfigStore {
    fn get(&self, key: &str) -> Option<String> {
        self.data.get(key).cloned()
    }
}

// Contract tests cannot run natively: `worker::Env` is only available inside
// the Cloudflare Workers runtime and has no testable mock. Platform-level
// contract coverage is provided by the smoke test
// (`scripts/smoke_test_config.sh cloudflare`) against a live wrangler dev instance.
