//! Axum adapter config store: reads from a per-id local JSON file.
//!
//! Each declared `[stores.config].ids` id maps to a file at
//! `.edgezero/local-config-<id>.json`. The file holds a JSON object of
//! `string -> string` pairs. Typed `config push --adapter axum` writes ONE
//! entry — the selected config key (defaults to the logical store id,
//! overridable with `--key`) keyed to a JSON-encoded `BlobEnvelope` string,
//! which the runtime `AppConfig<C>` extractor parses; hand-seeded flat
//! key/value files also work for raw `get`.
//!
//! If the file is absent the store is empty (`get` returns `Ok(None)` for
//! every key). This keeps `edgezero serve --adapter axum` permissive when
//! the project hasn't seeded any local config yet.

use std::collections::HashMap;
use std::env;
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
    /// The file must be a JSON object of `string -> string` pairs.
    /// Typed `config push --adapter axum` writes ONE entry — the selected
    /// config key (defaults to the logical store id, overridable with
    /// `--key`) keyed to a JSON-encoded `BlobEnvelope` string:
    ///
    /// ```json
    /// {
    ///   "app_config": "{\"version\":1,\"generated_at\":\"…\",\"sha256\":\"…\",\"data\":{}}"
    /// }
    /// ```
    ///
    /// The runtime `AppConfig<C>` extractor parses that envelope string;
    /// hand-seeded flat key/value files also work for raw `get`. Values
    /// must be strings — non-string values (`{"x": 42}`, nested objects,
    /// arrays) are rejected.
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
    ///
    /// Resolution order:
    ///
    /// 1. Walk up from the process cwd looking for an ancestor that
    ///    contains `edgezero.toml` (the manifest marker), the same
    ///    way cargo finds `Cargo.toml`. If found, return
    ///    `<ancestor>/.edgezero/local-config-<id>.json`.
    /// 2. Fall back to the cwd-relative `./.edgezero/local-config-<id>.json`.
    ///
    /// Why the walk-up: `edgezero config push --adapter axum` writes
    /// to `<manifest_root>/.edgezero/local-config-<id>.json`, but the
    /// axum runtime binary can legitimately be launched from any of
    /// the workspace root, the adapter crate dir, or an out-of-tree
    /// `cargo run` cwd. Without the walk-up, the runtime would read
    /// `<cwd>/.edgezero/...` and silently see an empty store
    /// whenever cwd doesn't happen to equal the manifest root.
    /// Walking up matches the directory model push uses, so the two
    /// always agree regardless of launch cwd.
    ///
    /// In a deployed binary (no `edgezero.toml` shipped alongside),
    /// the walk-up returns `None` and the cwd-relative fallback
    /// preserves the pre-fix behaviour. That deployment shape sets
    /// the cwd to where it dropped `.edgezero/` already, so the
    /// fallback is correct there too.
    #[must_use]
    #[inline]
    pub fn local_path(id: &str) -> PathBuf {
        let suffix = PathBuf::from(".edgezero").join(format!("local-config-{id}.json"));
        if let Some(root) = find_project_root_dir() {
            return root.join(suffix);
        }
        suffix
    }
}

#[async_trait(?Send)]
impl ConfigStore for AxumConfigStore {
    #[inline]
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        Ok(self.data.get(key).cloned())
    }
}

/// Walk up from the process cwd looking for an ancestor that
/// contains an `edgezero.toml` file (the manifest marker, same
/// convention cargo uses for `Cargo.toml`). Returns the first
/// matching ancestor, or `None` if the walk hits the filesystem
/// root without finding one.
///
/// Used by [`AxumConfigStore::local_path`] to keep push and runtime
/// on the same path regardless of launch cwd. Pulled out as a free
/// function so the same discovery rule can be reused by other
/// runtime helpers in the future.
fn find_project_root_dir() -> Option<PathBuf> {
    find_project_root_dir_from(&env::current_dir().ok()?)
}

/// Test-visible inner walk: same behaviour as
/// [`find_project_root_dir`] but with the starting directory passed
/// in explicitly so unit tests don't depend on the process cwd.
fn find_project_root_dir_from(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        if ancestor.join("edgezero.toml").is_file() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
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
    fn find_project_root_dir_from_returns_none_when_no_edgezero_toml_in_ancestors() {
        // Regression for the push/serve cwd mismatch: when the
        // launch cwd has no `edgezero.toml` anywhere up the chain
        // (e.g. a deployed binary in an isolated runtime image),
        // discovery must return None so `local_path` falls back to
        // cwd-relative `.edgezero/`. Pre-fix the runtime
        // unconditionally used `.edgezero/` relative to cwd, which
        // worked here too — confirm the fallback path is preserved.
        let temp = tempdir().expect("tempdir");
        assert!(
            find_project_root_dir_from(temp.path()).is_none(),
            "tempdir with no edgezero.toml must NOT match"
        );
    }

    #[test]
    fn find_project_root_dir_from_finds_ancestor_with_edgezero_toml() {
        // The fix: when an ancestor contains `edgezero.toml`,
        // discovery returns it. This is the case that breaks pre-
        // fix when serve runs from a crate dir but push wrote to
        // the workspace root.
        let temp = tempdir().expect("tempdir");
        fs::write(temp.path().join("edgezero.toml"), "").expect("write marker");
        // Simulate cwd two levels deep inside the project.
        let nested = temp.path().join("crates").join("my-app-adapter-axum");
        fs::create_dir_all(&nested).expect("nested dir");

        let resolved =
            find_project_root_dir_from(&nested).expect("ancestor with edgezero.toml must match");
        // Canonicalize both sides — on macOS `/tmp` is a symlink to
        // `/private/tmp`, which makes the raw tempdir path and the
        // resolved ancestor inequal byte-for-byte.
        assert_eq!(
            fs::canonicalize(&resolved).expect("canonicalize resolved"),
            fs::canonicalize(temp.path()).expect("canonicalize tempdir")
        );
    }

    #[test]
    fn find_project_root_dir_from_stops_at_first_match() {
        // If two ancestors both have `edgezero.toml`, pick the
        // nearest one — analogous to how cargo resolves
        // `Cargo.toml` workspace vs. package roots.
        let temp = tempdir().expect("tempdir");
        fs::write(temp.path().join("edgezero.toml"), "outer").expect("outer");
        let inner = temp.path().join("inner");
        fs::create_dir_all(&inner).expect("inner dir");
        fs::write(inner.join("edgezero.toml"), "inner").expect("inner marker");
        let nested = inner.join("deeper");
        fs::create_dir_all(&nested).expect("nested dir");

        let resolved = find_project_root_dir_from(&nested).expect("match");
        assert_eq!(
            fs::canonicalize(&resolved).expect("canonicalize resolved"),
            fs::canonicalize(&inner).expect("canonicalize inner")
        );
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
        // The path's TAIL is the stable contract; the prefix may
        // be cwd-relative (`./.edgezero/...`) or rooted at the
        // discovered project ancestor (`<root>/.edgezero/...`)
        // depending on whether the test runner's cwd has an
        // `edgezero.toml` ancestor. Both forms are correct — we
        // assert only on the suffix so the test doesn't flake when
        // someone adds an `edgezero.toml` at the workspace root.
        let path = AxumConfigStore::local_path("app_config");
        let suffix = PathBuf::from(".edgezero").join("local-config-app_config.json");
        assert!(
            path.ends_with(&suffix),
            "local_path must always end in `.edgezero/local-config-<id>.json`; got `{}`",
            path.display()
        );
    }
}
