//! Axum adapter config store: env vars with in-memory defaults fallback.

use std::collections::HashMap;

use edgezero_core::config_store::ConfigStore;

/// Config store for local dev / Axum. Reads from env vars with manifest
/// defaults as fallback. Env vars take precedence over defaults.
///
/// # Note on `from_env`
///
/// [`AxumConfigStore::from_env`] snapshots the **entire** process environment
/// at construction time. Any env var name is therefore accessible via
/// `ctx.config_store()?.get("VAR_NAME")`. In practice, manifest config keys
/// use lowercase dotted names (e.g. `feature.new_checkout`) which do not
/// collide with typical uppercase process vars (`PATH`, `HOME`, etc.), so
/// accidental leakage is unlikely. For production deployments use Fastly or
/// Cloudflare adapters, which read only from their respective platform stores.
pub struct AxumConfigStore {
    env: HashMap<String, String>,
    defaults: HashMap<String, String>,
}

impl AxumConfigStore {
    /// Create from env vars and optional manifest defaults.
    pub fn new(
        env: impl IntoIterator<Item = (String, String)>,
        defaults: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            env: env.into_iter().collect(),
            defaults: defaults.into_iter().collect(),
        }
    }

    /// Create from the current process environment and manifest defaults.
    pub fn from_env(defaults: impl IntoIterator<Item = (String, String)>) -> Self {
        Self::new(std::env::vars(), defaults)
    }
}

impl ConfigStore for AxumConfigStore {
    fn get(&self, key: &str) -> Option<String> {
        self.env
            .get(key)
            .or_else(|| self.defaults.get(key))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(env: &[(&str, &str)], defaults: &[(&str, &str)]) -> AxumConfigStore {
        AxumConfigStore::new(
            env.iter().map(|(k, v)| (k.to_string(), v.to_string())),
            defaults.iter().map(|(k, v)| (k.to_string(), v.to_string())),
        )
    }

    #[test]
    fn axum_config_store_returns_values() {
        let s = store(&[("MY_KEY", "my_val")], &[]);
        assert_eq!(s.get("MY_KEY"), Some("my_val".to_string()));
    }

    #[test]
    fn axum_config_store_returns_none_for_missing() {
        let s = store(&[], &[]);
        assert_eq!(s.get("NOPE"), None);
    }

    #[test]
    fn axum_config_store_env_overrides_defaults() {
        let s = store(&[("KEY", "from_env")], &[("KEY", "from_default")]);
        assert_eq!(s.get("KEY"), Some("from_env".to_string()));
    }

    #[test]
    fn axum_config_store_falls_back_to_defaults() {
        let s = store(&[], &[("KEY", "default_val")]);
        assert_eq!(s.get("KEY"), Some("default_val".to_string()));
    }

    // Run the shared contract tests against AxumConfigStore (env path).
    edgezero_core::config_store_contract_tests!(axum_config_store_env_contract, {
        AxumConfigStore::new(
            [
                ("contract.key.a".to_string(), "value_a".to_string()),
                ("contract.key.b".to_string(), "value_b".to_string()),
            ],
            [],
        )
    });

    // Run the shared contract tests against AxumConfigStore (defaults path).
    edgezero_core::config_store_contract_tests!(axum_config_store_defaults_contract, {
        AxumConfigStore::new(
            [],
            [
                ("contract.key.a".to_string(), "value_a".to_string()),
                ("contract.key.b".to_string(), "value_b".to_string()),
            ],
        )
    });
}
