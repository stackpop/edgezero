//! Axum adapter config store: env vars with in-memory defaults fallback.

use std::collections::HashMap;

use edgezero_core::config_store::{ConfigStore, ConfigStoreError};

/// Config store for local dev / Axum. Reads from env vars with manifest
/// defaults as fallback. Env vars take precedence over defaults.
///
/// # Note on `from_env`
///
/// [`AxumConfigStore::from_env`] only reads environment variables for keys
/// declared in `[stores.config.defaults]`. Use an empty-string default when a
/// key should be overrideable from env without carrying a real default value.
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
        Self::from_lookup(defaults, |key| std::env::var(key).ok())
    }

    fn from_lookup<F>(defaults: impl IntoIterator<Item = (String, String)>, mut lookup: F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        let defaults: HashMap<String, String> = defaults.into_iter().collect();
        let env = defaults
            .keys()
            .filter_map(|key| lookup(key).map(|value| (key.clone(), value)))
            .collect();
        Self { env, defaults }
    }
}

impl ConfigStore for AxumConfigStore {
    fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        Ok(self
            .env
            .get(key)
            .or_else(|| self.defaults.get(key))
            .cloned())
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
        assert_eq!(
            s.get("MY_KEY").expect("config value"),
            Some("my_val".to_string())
        );
    }

    #[test]
    fn axum_config_store_returns_none_for_missing() {
        let s = store(&[], &[]);
        assert_eq!(s.get("NOPE").expect("missing config"), None);
    }

    #[test]
    fn axum_config_store_env_overrides_defaults() {
        let s = store(&[("KEY", "from_env")], &[("KEY", "from_default")]);
        assert_eq!(
            s.get("KEY").expect("config value"),
            Some("from_env".to_string())
        );
    }

    #[test]
    fn axum_config_store_falls_back_to_defaults() {
        let s = store(&[], &[("KEY", "default_val")]);
        assert_eq!(
            s.get("KEY").expect("default config"),
            Some("default_val".to_string())
        );
    }

    #[test]
    fn axum_config_store_from_env_reads_only_declared_keys() {
        let s = AxumConfigStore::from_lookup(
            [
                ("feature.new_checkout".to_string(), "false".to_string()),
                ("service.timeout_ms".to_string(), "1500".to_string()),
            ],
            |key| match key {
                "feature.new_checkout" => Some("true".to_string()),
                "DATABASE_URL" => Some("postgres://secret".to_string()),
                _ => None,
            },
        );

        assert_eq!(
            s.get("feature.new_checkout").expect("allowed env override"),
            Some("true".to_string())
        );
        assert_eq!(
            s.get("service.timeout_ms").expect("default fallback"),
            Some("1500".to_string())
        );
        assert_eq!(
            s.get("DATABASE_URL")
                .expect("undeclared key should stay hidden"),
            None
        );
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
