//! Axum adapter config store: reads from a per-id local JSON file.
//!
//! Each declared `[stores.config].ids` id maps to a file at
//! `.edgezero/local-config-<id>.json` (§15 of the design spec). The file
//! holds a flat object of `string -> string` pairs — the same shape
//! `config push --adapter axum` will write when Stage 7 lands.
//!
//! If the file is absent the store is empty (`get` returns `Ok(None)` for
//! every key). This keeps `edgezero serve --adapter axum` permissive when
//! the project hasn't seeded any local config yet.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use edgezero_core::config_store::{ConfigStore, ConfigStoreError};

/// Local-file config store used by the Axum dev server.
///
/// Construction is fallible only when the backing file is present but
/// malformed JSON — a missing file is a documented "no values seeded yet"
/// state, not an error.
pub struct AxumConfigStore {
    data: HashMap<String, String>,
}

impl AxumConfigStore {
    fn empty() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Open the local-file config store for a given logical id.
    ///
    /// Reads `.edgezero/local-config-<id>.json` if present and parses it
    /// as a flat `string -> string` JSON object. A missing file yields an
    /// empty store. A malformed file yields
    /// [`ConfigStoreError::Unavailable`] so the dev-server log surfaces
    /// the problem at startup rather than at first request.
    ///
    /// # Errors
    /// Returns [`ConfigStoreError::Unavailable`] when the backing file
    /// exists but cannot be read or parsed.
    #[inline]
    pub fn from_local_file(id: &str) -> Result<Self, ConfigStoreError> {
        Self::from_path(&Self::local_path(id))
    }

    /// Build a store from an explicit `{key -> value}` map. Intended for
    /// tests and for callers that already have parsed config in memory.
    #[inline]
    pub fn from_map<E>(entries: E) -> Self
    where
        E: IntoIterator<Item = (String, String)>,
    {
        Self {
            data: entries.into_iter().collect(),
        }
    }

    /// Open the local-file config store at an explicit path
    /// (overrides the `.edgezero/local-config-<id>.json` default
    /// from [`Self::from_local_file`]). Intended for downstream
    /// integration tests that want to load a JSON payload written
    /// by `config push --adapter axum` to a tempdir, without
    /// changing the process CWD.
    ///
    /// Behaviour matches `from_local_file`: a missing file yields
    /// an empty store; a present-but-malformed file yields
    /// [`ConfigStoreError::Unavailable`].
    ///
    /// # Errors
    /// Returns [`ConfigStoreError::Unavailable`] when the file
    /// exists but cannot be read or parsed.
    #[inline]
    pub fn from_path(path: &Path) -> Result<Self, ConfigStoreError> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(Self::empty());
            }
            Err(err) => {
                return Err(ConfigStoreError::unavailable(format!(
                    "failed to read {}: {err}",
                    path.display()
                )));
            }
        };
        let data: HashMap<String, String> = serde_json::from_str(&raw).map_err(|err| {
            ConfigStoreError::unavailable(format!(
                "{} is not a flat string -> string JSON object: {err}",
                path.display()
            ))
        })?;
        Ok(Self { data })
    }

    /// Resolve the on-disk path for the given logical config id.
    #[must_use]
    #[inline]
    pub fn local_path(id: &str) -> PathBuf {
        PathBuf::from(".edgezero").join(format!("local-config-{id}.json"))
    }
}

#[async_trait(?Send)]
impl ConfigStore for AxumConfigStore {
    #[inline]
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        Ok(self.data.get(key).cloned())
    }
}

#[cfg(test)]
mod tests {
    // Run the shared contract tests against AxumConfigStore.
    edgezero_core::config_store_contract_tests!(axum_config_store_contract, {
        AxumConfigStore::from_map([
            ("contract.key.a".to_owned(), "value_a".to_owned()),
            ("contract.key.b".to_owned(), "value_b".to_owned()),
        ])
    });

    use super::*;
    use futures::executor::block_on;
    use tempfile::tempdir;

    #[test]
    fn axum_config_store_from_map_returns_values() {
        let cs = AxumConfigStore::from_map([("greeting".to_owned(), "hello".to_owned())]);
        assert_eq!(
            block_on(cs.get("greeting")).expect("config value"),
            Some("hello".to_owned())
        );
        assert_eq!(block_on(cs.get("missing")).expect("missing config"), None);
    }

    #[test]
    fn axum_config_store_from_path_returns_empty_for_missing_file() {
        let temp = tempdir().expect("tempdir");
        let cs = AxumConfigStore::from_path(&temp.path().join("nope.json"))
            .expect("missing file is permissive");
        assert_eq!(block_on(cs.get("anything")).expect("empty store"), None);
    }

    #[test]
    fn axum_config_store_from_path_reads_flat_json() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("local-config-app_config.json");
        fs::write(
            &path,
            r#"{"greeting":"hello from file","feature.new_checkout":"false"}"#,
        )
        .expect("write json");

        let cs = AxumConfigStore::from_path(&path).expect("parse json");
        assert_eq!(
            block_on(cs.get("greeting")).expect("value"),
            Some("hello from file".to_owned())
        );
        assert_eq!(
            block_on(cs.get("feature.new_checkout")).expect("dotted value"),
            Some("false".to_owned())
        );
        assert_eq!(block_on(cs.get("missing")).expect("missing"), None);
    }

    #[test]
    fn axum_config_store_from_path_rejects_malformed_json() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("local-config-bad.json");
        fs::write(&path, "{not json}").expect("write");

        match AxumConfigStore::from_path(&path) {
            Err(ConfigStoreError::Unavailable { .. }) => {}
            Err(other) => panic!("expected Unavailable, got {other:?}"),
            Ok(_) => panic!("malformed JSON must surface as error"),
        }
    }

    #[test]
    fn axum_config_store_from_path_rejects_non_string_values() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("local-config-numeric.json");
        fs::write(&path, r#"{"greeting":42}"#).expect("write");

        match AxumConfigStore::from_path(&path) {
            Err(ConfigStoreError::Unavailable { .. }) => {}
            Err(other) => panic!("expected Unavailable, got {other:?}"),
            Ok(_) => panic!("non-string values must surface as error"),
        }
    }

    #[test]
    fn local_path_is_keyed_by_logical_id() {
        let path = AxumConfigStore::local_path("app_config");
        assert_eq!(
            path,
            PathBuf::from(".edgezero/local-config-app_config.json")
        );
    }
}
