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
use std::path::{Path, PathBuf, absolute};

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
        // Reject a symlinked final component before reading. `config push
        // --adapter axum` / provision refuse to WRITE these files through a
        // symlink; a legitimately-provisioned local config is therefore
        // never a symlink, so one appearing here is anomalous. Rejecting it
        // on the read path too keeps a single consistent final-path policy
        // and stops a planted symlink from redirecting the runtime read.
        if fs::symlink_metadata(path).is_ok_and(|md| md.file_type().is_symlink()) {
            return Err(ConfigStoreError::unavailable(format!(
                "refusing to read `{}`: it is a symlink; EdgeZero-owned local config is never a symlink",
                path.display()
            )));
        }
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

/// Resolve the project root that anchors the runtime `.edgezero`
/// directory.
///
/// When `EDGEZERO_MANIFEST` is set (the manifest path can be renamed
/// away from the default `edgezero.toml`), the root is that manifest's
/// parent directory — the SAME root `provision --local` resolves the
/// manifest from — so runtime reads land exactly where provisioning
/// wrote. Only when the variable is unset does discovery fall back to
/// the upward `edgezero.toml` search below.
///
/// Used by [`AxumConfigStore::local_path`] to keep push and runtime
/// on the same path regardless of launch cwd. Also reused by the dev
/// server's KV path anchoring so config and KV state land in the SAME
/// `.edgezero` directory.
pub(crate) fn find_project_root_dir() -> Option<PathBuf> {
    if let Some(root) = manifest_root_from_env() {
        return Some(root);
    }
    find_project_root_dir_from(&env::current_dir().ok()?)
}

/// Derive the project root from an explicitly set `EDGEZERO_MANIFEST`.
///
/// Mirrors the CLI's `manifest_root_from` — the manifest's parent is
/// the project root — but resolves a bare or relative manifest to an
/// ABSOLUTE path against the current working directory FIRST, so the
/// derived root stays stable regardless of a later cwd change. The Axum
/// runtime relaunches the child with cwd set to the adapter crate dir
/// (see `cli/run.rs`) before resolving `.edgezero`; a relative `.` root
/// would then resolve against the adapter crate dir instead of the
/// project root where `provision --local` wrote. Anchoring on the
/// launch cwd up front keeps runtime reads and provision writes on the
/// same directory. Returns `None` when the variable is unset (so the
/// caller falls back to the `edgezero.toml` upward search) or when the
/// current directory can't be determined.
fn manifest_root_from_env() -> Option<PathBuf> {
    let manifest = absolute(env::var("EDGEZERO_MANIFEST").ok()?).ok()?;
    let root = manifest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    Some(root)
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
    use edgezero_core::test_env::{EnvOverride, env_lock};
    use futures::executor::block_on;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[test]
    fn from_path_rejects_symlinked_config_file() {
        use std::os::unix::fs::symlink;
        // A planted symlink where the local config is expected must be
        // refused on the READ path, matching the write side.
        let temp = tempdir().expect("tempdir");
        let real = temp.path().join("real.json");
        fs::write(&real, "{\"k\":\"v\"}").expect("write real");
        let link = temp.path().join("local-config-app.json");
        symlink(&real, &link).expect("symlink");
        let Err(err) = AxumConfigStore::from_path(&link) else {
            panic!("symlinked config must be refused, not silently followed");
        };
        assert!(
            matches!(err, ConfigStoreError::Unavailable { .. }),
            "symlinked config is Unavailable"
        );
    }

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
    fn find_project_root_dir_honors_renamed_manifest_env() {
        // A renamed top-level manifest (`EDGEZERO_MANIFEST=<dir>/project.local.toml`)
        // must anchor the runtime `.edgezero` directory on that
        // manifest's parent — the same root `provision --local` writes
        // under — NOT the `edgezero.toml` upward-search fallback. Note
        // the tempdir deliberately has NO `edgezero.toml`, so a fallback
        // would resolve somewhere else entirely.
        let _lock = env_lock().lock().expect("env lock");
        let temp = tempdir().expect("tempdir");
        let manifest = temp.path().join("project.local.toml");
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest);

        let resolved = find_project_root_dir().expect("manifest env must resolve a root");
        assert_eq!(
            fs::canonicalize(&resolved).expect("canonicalize resolved"),
            fs::canonicalize(temp.path()).expect("canonicalize tempdir"),
            "runtime root must be the renamed manifest's parent directory"
        );
    }

    #[test]
    fn find_project_root_dir_anchors_bare_manifest_env_to_cwd() {
        // A BARE relative `EDGEZERO_MANIFEST=project.local.toml` must
        // anchor the runtime `.edgezero` on the ABSOLUTE launch cwd, not
        // a relative `.`. The Axum runtime relaunches the child with cwd
        // set to the adapter crate dir before resolving `.edgezero`, so a
        // relative `.` root would drift to that crate dir instead of the
        // project root where `provision --local` wrote. Resolving the
        // manifest to absolute up front keeps the two in agreement
        // regardless of the later cwd change. Setting cwd in a test is
        // racy, so assert on absoluteness + equality with the current
        // cwd under the env lock rather than mutating cwd.
        let _lock = env_lock().lock().expect("env lock");
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", "project.local.toml");

        let resolved = find_project_root_dir().expect("bare manifest env must resolve a root");
        assert!(
            resolved.is_absolute(),
            "bare manifest env must resolve an ABSOLUTE root, got `{}`",
            resolved.display()
        );
        let cwd = env::current_dir().expect("cwd");
        assert_eq!(
            fs::canonicalize(&resolved).expect("canonicalize resolved"),
            fs::canonicalize(&cwd).expect("canonicalize cwd"),
            "bare manifest env must anchor on the absolute launch cwd, not `.`"
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
