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

use std::env;
use std::fs;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use edgezero_core::config_store::{ConfigStore, ConfigStoreError};
use edgezero_core::{BoundedStoreRead, Deadline, MonotonicClock};

use crate::config_snapshot::Snapshot;
use crate::config_store_limits::ConfigStoreLimits;

/// Local-file config store used by the Axum dev server.
///
/// Construction validates finite allocation budgets and the flat-string shape.
/// A missing file is an empty declared snapshot, not an absent binding.
pub struct AxumConfigStore {
    data: Snapshot,
    #[cfg(test)]
    unbounded_get_calls: AtomicUsize,
}

impl AxumConfigStore {
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
    /// exists but cannot be read or parsed, and [`ConfigStoreError::ValueTooLarge`]
    /// for file, entry, string or allocation-budget violations.
    #[inline]
    pub fn from_local_file(id: &str, limits: ConfigStoreLimits) -> Result<Self, ConfigStoreError> {
        Self::from_path(&Self::local_path(id), limits)
    }

    /// Build a store from an explicit `{key -> value}` map. Intended for
    /// tests and for callers that already have parsed config in memory.
    /// Caller-owned input allocations are outside the snapshot's allocation charge.
    ///
    /// # Errors
    /// Returns a typed size error for allocation limits, or unavailable for duplicates.
    #[inline]
    pub fn from_map<E>(entries: E, limits: ConfigStoreLimits) -> Result<Self, ConfigStoreError>
    where
        E: IntoIterator<Item = (String, String)>,
    {
        Ok(Self {
            data: Snapshot::from_entries(entries, limits)?,
            #[cfg(test)]
            unbounded_get_calls: AtomicUsize::new(0),
        })
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
    /// exists but cannot be read or parsed, and [`ConfigStoreError::ValueTooLarge`]
    /// for file, entry, string or allocation-budget violations.
    #[inline]
    pub fn from_path(path: &Path, limits: ConfigStoreLimits) -> Result<Self, ConfigStoreError> {
        Self::load_at_startup(path, limits, 0)
    }

    pub(crate) fn load_at_startup(
        path: &Path,
        limits: ConfigStoreLimits,
        existing: usize,
    ) -> Result<Self, ConfigStoreError> {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NONBLOCK);
        let data = match options.open(path) {
            Ok(mut file) => {
                let metadata = file.metadata().map_err(|_io| {
                    ConfigStoreError::unavailable("config snapshot metadata unavailable")
                })?;
                if !metadata.is_file() {
                    return Err(ConfigStoreError::unavailable(
                        "config snapshot requires a regular file",
                    ));
                }
                if metadata.len()
                    > u64::try_from(limits.max_file_bytes())
                        .map_err(|_size| ConfigStoreError::ValueTooLarge)?
                {
                    return Err(ConfigStoreError::ValueTooLarge);
                }
                Snapshot::load(&mut file, limits, existing)?
            }
            Err(err) if err.kind() == ErrorKind::NotFound => Snapshot::empty(limits, existing)?,
            Err(_io) => {
                return Err(ConfigStoreError::unavailable("config snapshot open failed"));
            }
        };
        Ok(Self {
            data,
            #[cfg(test)]
            unbounded_get_calls: AtomicUsize::new(0),
        })
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

    /// Charged snapshot index, key and shared-payload requested allocation bytes.
    #[inline]
    #[must_use]
    pub const fn resident_allocation_bytes(&self) -> usize {
        self.data.resident_bytes()
    }
}

#[async_trait(?Send)]
impl ConfigStore for AxumConfigStore {
    #[inline]
    async fn get(&self, key: &str) -> Result<Option<edgezero_core::ConfigValue>, ConfigStoreError> {
        #[cfg(test)]
        self.unbounded_get_calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.data.get(key).cloned())
    }

    #[inline]
    async fn get_bounded(
        &self,
        key: &str,
        clock: &MonotonicClock,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<edgezero_core::ConfigValue>, ConfigStoreError> {
        if deadline.is_expired_at(clock.now()) {
            return Err(ConfigStoreError::DeadlineExceeded);
        }

        // Payloads are already bounded resident snapshots; reads clone only Arc.
        let stored_value = self.data.get(key);
        if deadline.is_expired_at(clock.now()) {
            return Err(ConfigStoreError::DeadlineExceeded);
        }

        let backend_bytes = stored_value.map_or(Ok(0_u64), |value| {
            u64::try_from(value.len()).map_err(|_length_error| ConfigStoreError::ValueTooLarge)
        })?;
        if backend_bytes > max_backend_bytes || backend_bytes > max_value_bytes {
            return Err(ConfigStoreError::ValueTooLarge);
        }

        let value = stored_value.cloned();
        if deadline.is_expired_at(clock.now()) {
            return Err(ConfigStoreError::DeadlineExceeded);
        }

        Ok(BoundedStoreRead {
            backend_bytes,
            value,
        })
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
        AxumConfigStore::from_map(
            [
                ("contract.key.a".to_owned(), "value_a".to_owned()),
                ("contract.key.b".to_owned(), "value_b".to_owned()),
            ],
            ConfigStoreLimits::default(),
        )
        .expect("snapshot")
    });

    use super::*;
    use edgezero_core::{Deadline, MonotonicClock, MonotonicInstant};
    use futures::executor::block_on;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn bounded_get_shares_payload_across_reads() {
        let store = AxumConfigStore::from_map(
            [("key".to_owned(), "payload".to_owned())],
            ConfigStoreLimits::default(),
        )
        .expect("snapshot");
        let clock = MonotonicClock::default();
        let deadline = Deadline::after(Duration::from_secs(1));
        let first = block_on(store.get_bounded("key", &clock, deadline, 7, 7))
            .expect("first read")
            .value
            .expect("first value");
        let second = block_on(store.get_bounded("key", &clock, deadline, 7, 7))
            .expect("second read")
            .value
            .expect("second value");
        assert_eq!(
            first.as_ptr(),
            second.as_ptr(),
            "reads must share the snapshot payload"
        );
    }

    #[test]
    fn startup_rejects_keys_over_the_default_allocation_limit() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("config.json");
        let key = "k".repeat(1025);
        fs::write(&path, serde_json::json!({key: "value"}).to_string()).expect("write");
        assert!(matches!(
            AxumConfigStore::from_path(&path, ConfigStoreLimits::default()),
            Err(ConfigStoreError::ValueTooLarge)
        ));
    }

    #[test]
    fn startup_rejects_duplicate_keys_instead_of_overwriting() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("config.json");
        fs::write(&path, r#"{"key":"first","key":"second"}"#).expect("write duplicate-key fixture");
        assert!(matches!(
            AxumConfigStore::from_path(&path, ConfigStoreLimits::default()),
            Err(ConfigStoreError::Unavailable { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn named_pipe_is_rejected_without_waiting_for_a_writer() {
        use std::process::Command;
        use std::sync::mpsc::{self, RecvTimeoutError};
        use std::thread;
        let directory = tempdir().expect("directory");
        let path = directory.path().join("config.fifo");
        assert!(
            Command::new("mkfifo")
                .arg(&path)
                .status()
                .expect("mkfifo")
                .success()
        );
        let reader_path = path.clone();
        let (sender, receiver) = mpsc::channel();
        let loader = thread::spawn(move || {
            sender
                .send(AxumConfigStore::from_path(&reader_path, ConfigStoreLimits::default()).err())
                .expect("send");
        });
        let result = receiver.recv_timeout(Duration::from_secs(2));
        if matches!(result, Err(RecvTimeoutError::Timeout)) {
            // Unblock a regressed blocking open before failing, so the test leaks no thread.
            drop(
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .expect("unblock reader"),
            );
        }
        loader.join().expect("loader");
        assert!(matches!(
            result.expect("nonblocking rejection"),
            Some(ConfigStoreError::Unavailable { .. })
        ));
    }

    #[test]
    fn bounded_get_accepts_exact_caps_and_reports_exact_backend_bytes() {
        let cs = AxumConfigStore::from_map(
            [("greeting".to_owned(), "hello".to_owned())],
            ConfigStoreLimits::default(),
        )
        .expect("snapshot");
        let clock = MonotonicClock::default();

        let read = block_on(cs.get_bounded(
            "greeting",
            &clock,
            Deadline::after(Duration::from_secs(1)),
            5,
            5,
        ))
        .expect("exact cap must succeed");

        assert_eq!(read.backend_bytes, 5);
        assert_eq!(read.value.as_deref(), Some("hello"));
    }

    #[test]
    fn bounded_get_rejects_either_cap_without_using_cloning_get() {
        for (max_backend_bytes, max_value_bytes) in [(4, 5), (5, 4)] {
            let cs = AxumConfigStore::from_map(
                [("greeting".to_owned(), "hello".to_owned())],
                ConfigStoreLimits::default(),
            )
            .expect("snapshot");
            let clock = MonotonicClock::default();

            let error = block_on(cs.get_bounded(
                "greeting",
                &clock,
                Deadline::after(Duration::from_secs(1)),
                max_backend_bytes,
                max_value_bytes,
            ))
            .expect_err("over-cap value must fail");

            assert!(matches!(error, ConfigStoreError::ValueTooLarge));
            assert_eq!(cs.unbounded_get_calls.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn bounded_get_rejects_an_expired_deadline_before_reading() {
        let cs = AxumConfigStore::from_map(
            [("greeting".to_owned(), "hello".to_owned())],
            ConfigStoreLimits::default(),
        )
        .expect("snapshot");
        let expired = Deadline::at_instant(MonotonicInstant::now());
        let clock = MonotonicClock::default();

        let error = block_on(cs.get_bounded("greeting", &clock, expired, 5, 5))
            .expect_err("expired deadline must fail");

        assert!(matches!(error, ConfigStoreError::DeadlineExceeded));
        assert_eq!(cs.unbounded_get_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn bounded_config_read_uses_injected_clock() {
        let cs = AxumConfigStore::from_map(
            [("greeting".to_owned(), "hello".to_owned())],
            ConfigStoreLimits::default(),
        )
        .expect("snapshot");
        let process_now = MonotonicInstant::now();
        let injected_now = process_now
            .checked_sub(Duration::from_mins(1))
            .expect("injected instant");
        let clock = MonotonicClock::new(move || injected_now);
        let deadline = Deadline::at_instant(
            injected_now
                .checked_add(Duration::from_secs(1))
                .expect("deadline instant"),
        );

        let read = block_on(cs.get_bounded("greeting", &clock, deadline, 5, 5))
            .expect("the application clock is still before its deadline");

        assert_eq!(read.value.as_deref(), Some("hello"));
    }

    #[test]
    fn axum_config_store_from_map_returns_values() {
        let cs = AxumConfigStore::from_map(
            [("greeting".to_owned(), "hello".to_owned())],
            ConfigStoreLimits::default(),
        )
        .expect("snapshot");
        assert_eq!(
            block_on(cs.get("greeting")).expect("config value"),
            Some(edgezero_core::ConfigValue::from("hello"))
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
        let cs = AxumConfigStore::from_path(
            &temp.path().join("nope.json"),
            ConfigStoreLimits::default(),
        )
        .expect("missing file is permissive");
        assert_eq!(block_on(cs.get("anything")).expect("empty store"), None);
    }

    #[test]
    fn axum_config_store_from_path_reads_flat_json() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("local-config-app_config.json");
        fs::write(
            &path,
            serde_json::json!({"greeting": "hello from file", "feature.new_checkout": "false"})
                .to_string(),
        )
        .expect("write json");

        let cs =
            AxumConfigStore::from_path(&path, ConfigStoreLimits::default()).expect("parse json");
        assert_eq!(
            block_on(cs.get("greeting")).expect("value"),
            Some(edgezero_core::ConfigValue::from("hello from file"))
        );
        assert_eq!(
            block_on(cs.get("feature.new_checkout")).expect("dotted value"),
            Some(edgezero_core::ConfigValue::from("false"))
        );
        assert_eq!(block_on(cs.get("missing")).expect("missing"), None);
    }

    #[test]
    fn axum_config_store_from_path_rejects_malformed_json() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("local-config-bad.json");
        fs::write(&path, "{not json}").expect("write");

        match AxumConfigStore::from_path(&path, ConfigStoreLimits::default()) {
            Err(ConfigStoreError::Unavailable { .. }) => {}
            Err(other) => panic!("expected Unavailable, got {other:?}"),
            Ok(_) => panic!("malformed JSON must surface as error"),
        }
    }

    #[test]
    fn axum_config_store_from_path_rejects_non_string_values() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("local-config-numeric.json");
        fs::write(&path, serde_json::json!({"greeting": 42_u32}).to_string()).expect("write");

        match AxumConfigStore::from_path(&path, ConfigStoreLimits::default()) {
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
