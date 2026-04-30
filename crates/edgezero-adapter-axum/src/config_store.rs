//! Axum adapter config store: env vars with in-memory defaults fallback.

use std::collections::HashMap;
use std::env;

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
    defaults: HashMap<String, String>,
    env: HashMap<String, String>,
}

impl AxumConfigStore {
    /// Create from the current process environment and manifest defaults.
    #[inline]
    pub fn from_env<D>(defaults: D) -> Self
    where
        D: IntoIterator<Item = (String, String)>,
    {
        Self::from_lookup(defaults, |key| env::var(key).ok())
    }

    fn from_lookup<D, F>(defaults: D, mut lookup: F) -> Self
    where
        D: IntoIterator<Item = (String, String)>,
        F: FnMut(&str) -> Option<String>,
    {
        let collected: HashMap<String, String> = defaults.into_iter().collect();
        let env = collected
            .keys()
            .filter_map(|key| lookup(key).map(|value| (key.clone(), value)))
            .collect();
        Self {
            defaults: collected,
            env,
        }
    }

    /// Create from env vars and optional manifest defaults.
    #[inline]
    pub fn new<E, D>(env: E, defaults: D) -> Self
    where
        E: IntoIterator<Item = (String, String)>,
        D: IntoIterator<Item = (String, String)>,
    {
        Self {
            defaults: defaults.into_iter().collect(),
            env: env.into_iter().collect(),
        }
    }
}

impl ConfigStore for AxumConfigStore {
    #[inline]
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
    // Run the shared contract tests against AxumConfigStore (defaults path).
    edgezero_core::config_store_contract_tests!(axum_config_store_defaults_contract, {
        AxumConfigStore::new(
            [],
            [
                ("contract.key.a".to_owned(), "value_a".to_owned()),
                ("contract.key.b".to_owned(), "value_b".to_owned()),
            ],
        )
    });

    // Run the shared contract tests against AxumConfigStore (env path).
    edgezero_core::config_store_contract_tests!(axum_config_store_env_contract, {
        AxumConfigStore::new(
            [
                ("contract.key.a".to_owned(), "value_a".to_owned()),
                ("contract.key.b".to_owned(), "value_b".to_owned()),
            ],
            [],
        )
    });

    use super::*;

    fn store(env: &[(&str, &str)], defaults: &[(&str, &str)]) -> AxumConfigStore {
        AxumConfigStore::new(
            env.iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
            defaults
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        )
    }

    #[test]
    fn axum_config_store_env_overrides_defaults() {
        let cs = store(&[("KEY", "from_env")], &[("KEY", "from_default")]);
        assert_eq!(
            cs.get("KEY").expect("config value"),
            Some("from_env".to_owned())
        );
    }

    #[test]
    fn axum_config_store_falls_back_to_defaults() {
        let cs = store(&[], &[("KEY", "default_val")]);
        assert_eq!(
            cs.get("KEY").expect("default config"),
            Some("default_val".to_owned())
        );
    }

    #[test]
    fn axum_config_store_from_env_reads_only_declared_keys() {
        let cs = AxumConfigStore::from_lookup(
            [
                ("feature.new_checkout".to_owned(), "false".to_owned()),
                ("service.timeout_ms".to_owned(), "1500".to_owned()),
            ],
            |key| match key {
                "feature.new_checkout" => Some("true".to_owned()),
                "DATABASE_URL" => Some("postgres://secret".to_owned()),
                _ => None,
            },
        );

        assert_eq!(
            cs.get("feature.new_checkout")
                .expect("allowed env override"),
            Some("true".to_owned())
        );
        assert_eq!(
            cs.get("service.timeout_ms").expect("default fallback"),
            Some("1500".to_owned())
        );
        assert_eq!(
            cs.get("DATABASE_URL")
                .expect("undeclared key should stay hidden"),
            None
        );
    }

    #[test]
    fn axum_config_store_returns_none_for_missing() {
        let cs = store(&[], &[]);
        assert_eq!(cs.get("NOPE").expect("missing config"), None);
    }

    #[test]
    fn axum_config_store_returns_values() {
        let cs = store(&[("MY_KEY", "my_val")], &[]);
        assert_eq!(
            cs.get("MY_KEY").expect("config value"),
            Some("my_val".to_owned())
        );
    }
}
