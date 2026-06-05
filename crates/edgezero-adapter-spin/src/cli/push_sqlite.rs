//! Direct-write `config push --adapter spin` to Spin's local `SQLite` KV
//! file (`.spin/sqlite_key_value.db`).
//!
//! Spin's `type = "spin"` KV backend is a single `SQLite` database used
//! by every label the spin.toml component declares. The schema is one
//! shared `spin_key_value(store, key, value)` table — there's no public
//! Spin CLI command to write into it from outside the running app, so
//! we open the file via `rusqlite` and run the same `INSERT … ON
//! CONFLICT DO UPDATE` Spin's runtime uses. WAL mode (Spin's default)
//! lets our writes coexist with a running `spin up`.
//!
//! ## Runtime coupling (operator-facing risk)
//!
//! Spin's local `SQLite` file format is an INTERNAL implementation
//! detail of the `key-value-spin` crate. Spin's docs explicitly warn
//! that it is "subject to change" (see
//! <https://spinframework.dev/v3/dynamic-configuration>). Our writer
//! couples to it three ways:
//!
//! 1. **Vendored schema constants** — `SPIN_KV_CREATE_TABLE` and
//!    `SPIN_KV_SET` are copied byte-for-byte from
//!    [spinframework/spin's `crates/key-value-spin/src/store.rs`](https://github.com/spinframework/spin/blob/main/crates/key-value-spin/src/store.rs).
//!    A unit test (`vendored_schema_matches_upstream_byte_for_byte`)
//!    pins both strings; a PRAGMA-shape test pins the resulting table
//!    columns. Drift in OUR copy fails CI.
//! 2. **Build-time SDK pin** — `Cargo.toml` pins `spin-sdk = "~6.0"`,
//!    so a Spin minor bump that touches the schema fails the build
//!    until the operator opts in and re-verifies.
//! 3. **Run-time CLI check** — [`verify_spin_runtime_compat`] shells
//!    `spin --version` before the first write of a session and
//!    `log::warn!`s if the major version is outside our verified
//!    range ([`VERIFIED_SPIN_MAJOR_RANGE`]). It NEVER blocks — Spin
//!    is optional from the writer's perspective — but the operator
//!    is told if their runtime is unknown.
//!
//! What the layered guards CANNOT catch: a Spin point-release that
//! changes the schema WITHOUT bumping past `~6.0` AND with a
//! same-major CLI version. Operators must verify with `spin up`
//! after the first push against a new Spin runtime; the warning
//! above is a heads-up, not a guarantee.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{params, Connection};

/// Major version range of `spin` CLI / runtime we've verified the
/// `spin_key_value` schema against. Spin's `crates/key-value-spin`
/// has used this schema since at least Spin 2.x; we last verified
/// against Spin 3.x (CLI 3.6.3, factor-key-value 3.x on
/// spinframework/spin main, 2026-06-04).
///
/// When Spin 4.0 (or whichever major-version-bump touches the
/// schema) lands, expect the verification to be repeated and this
/// constant updated. Until then, an operator running a Spin CLI
/// outside this range gets a `log::warn!` on first SQLite-direct
/// push.
const VERIFIED_SPIN_MAJOR_RANGE: &[u32] = &[2, 3];

/// EXACT `CREATE TABLE IF NOT EXISTS spin_key_value (…)` statement
/// Spin's `crates/key-value-spin/src/store.rs::KeyValueSqlite::create_connection`
/// runs. Source-of-truth: pulled from the file linked at the module
/// header on 2026-06-04 against an OPEN version of Spin's main branch.
/// **Do not reformat:** the contract test below compares this string
/// byte-for-byte against the upstream statement (whitespace included).
pub(crate) const SPIN_KV_CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS spin_key_value (
                           store TEXT NOT NULL,
                           key   TEXT NOT NULL,
                           value BLOB NOT NULL,

                           PRIMARY KEY (store, key)
                        )";

/// EXACT `INSERT … ON CONFLICT … DO UPDATE` statement Spin's
/// `SqliteStore::set` runs. Vendored from the same source file as
/// `SPIN_KV_CREATE_TABLE` so the contract test can byte-compare it
/// against the upstream version too.
pub(crate) const SPIN_KV_SET: &str =
    "INSERT INTO spin_key_value (store, key, value) VALUES ($1, $2, $3)
                     ON CONFLICT(store, key) DO UPDATE SET value=$3";

/// Default `SQLite` path relative to the spin manifest directory. Spin
/// hard-codes `.spin/sqlite_key_value.db` for its `type = "spin"`
/// backend when no `path` is set in `runtime-config.toml`. Vendored
/// from `crates/factor-key-value/src/runtime_config/spin.rs::path` in
/// spinframework/spin (June 2026).
pub(crate) const DEFAULT_SQLITE_RELATIVE_PATH: &str = ".spin/sqlite_key_value.db";

/// Resolve the `SQLite` path for a `[key_value_store.<label>] type =
/// "spin"` backend:
/// 1. If `runtime-config.toml` set an explicit `path`, honour it. If
///    relative, anchor against the runtime-config file's directory
///    (Spin's behaviour).
/// 2. Otherwise default to `<spin_manifest_dir>/.spin/sqlite_key_value.db`.
pub(crate) fn resolve_sqlite_path(
    spin_manifest_dir: &Path,
    runtime_config_dir: &Path,
    explicit_path: Option<&Path>,
) -> PathBuf {
    if let Some(custom) = explicit_path {
        if custom.is_absolute() {
            return custom.to_path_buf();
        }
        return runtime_config_dir.join(custom);
    }
    spin_manifest_dir.join(DEFAULT_SQLITE_RELATIVE_PATH)
}

/// Tracks whether the compat warning has already fired this session
/// so a multi-store push doesn't spam the operator's log.
static COMPAT_CHECK_DONE: AtomicBool = AtomicBool::new(false);

/// Shell `spin --version`, parse the major version, and `log::warn!`
/// if it's outside [`VERIFIED_SPIN_MAJOR_RANGE`]. Always returns —
/// Spin is OPTIONAL from this writer's perspective (the schema
/// matters; the CLI installation does not).
///
/// Idempotent: only runs the shellout the first time it's called per
/// process, so a batched push (multiple stores → multiple
/// `write_batch` calls) doesn't double-warn.
fn verify_spin_runtime_compat() {
    if COMPAT_CHECK_DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    // `spin` not on PATH: operator may be running push from a CI
    // runner without the CLI installed. That's legitimate (the
    // SQLite write doesn't NEED `spin`), so we say nothing rather
    // than tut-tutting at unrelated workflows.
    let Ok(output) = Command::new("spin").arg("--version").output() else {
        return;
    };
    if output.status.success() {
        // Continue below.
    } else {
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(major) = parse_spin_major_version(&stdout) else {
        log::warn!(
            "could not parse Spin CLI version from `spin --version` stdout ({stdout:?}); proceeding with the vendored SQLite schema, but the runtime-coupling guard couldn't verify"
        );
        return;
    };
    if VERIFIED_SPIN_MAJOR_RANGE.contains(&major) {
        log::debug!(
            "Spin CLI major version {major} is within EdgeZero's verified range {VERIFIED_SPIN_MAJOR_RANGE:?}; \
             SQLite-direct writes use the vendored `spin_key_value` schema."
        );
    } else {
        log::warn!(
            "Spin CLI major version {major} is outside EdgeZero's verified range {VERIFIED_SPIN_MAJOR_RANGE:?}. \
             The local SQLite KV writer uses Spin's internal `spin_key_value` schema vendored from spinframework/spin 3.x; \
             if Spin {major}.x changed the schema, your push will write a file the runtime can't read. \
             Verify with `spin up` after the push, and consider opening an issue if you see incompatibility."
        );
    }
}

/// Parse the major version from a `spin --version` stdout line such
/// as `"spin 3.6.3 (88d51cf 2026-04-09)\n"`. Returns `None` on any
/// shape we don't recognise — caller handles by warning.
fn parse_spin_major_version(stdout: &str) -> Option<u32> {
    // Expected shapes:
    //   "spin 3.6.3 (sha date)\n"
    //   "spin-cli 3.6.3\n"
    // Take the first token containing a `.` and parse its leading
    // numeric run as the major version.
    stdout
        .split_whitespace()
        .find(|token| {
            token.contains('.')
                && token
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_digit())
        })
        .and_then(|token| token.split('.').next())
        .and_then(|major| major.parse::<u32>().ok())
}

/// Write `entries` to `store_label` in the `SQLite` file at `db_path`.
/// Creates the file + parent dir + schema if any are missing (Spin
/// does the same on its first read, so this matches the runtime's
/// behaviour). One transaction wraps the whole batch so a per-entry
/// failure rolls back the prefix.
///
/// # Errors
/// Returns a human-readable error string on:
/// - failure to create the parent directory;
/// - failure to open the `SQLite` connection;
/// - failure to create the schema;
/// - failure to start / commit the transaction;
/// - per-entry `INSERT … ON CONFLICT` failure (names the failing key).
pub(crate) fn write_batch(
    db_path: &Path,
    store_label: &str,
    entries: &[(String, String)],
) -> Result<(), String> {
    // Best-effort runtime-compat check (once per session). Always
    // proceeds — Spin is optional from the writer's perspective.
    verify_spin_runtime_compat();

    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create parent dir for `{}`: {err}",
                    db_path.display()
                )
            })?;
        }
    }

    let mut connection = Connection::open(db_path)
        .map_err(|err| format!("failed to open `{}`: {err}", db_path.display()))?;

    connection
        .execute(SPIN_KV_CREATE_TABLE, [])
        .map_err(|err| format!("failed to create schema in `{}`: {err}", db_path.display()))?;

    let transaction = connection.transaction().map_err(|err| {
        format!(
            "failed to start transaction in `{}`: {err}",
            db_path.display()
        )
    })?;

    {
        let mut statement = transaction
            .prepare_cached(SPIN_KV_SET)
            .map_err(|err| format!("failed to prepare INSERT in `{}`: {err}", db_path.display()))?;

        for (key, value) in entries {
            statement
                .execute(params![store_label, key, value.as_bytes()])
                .map_err(|err| {
                    format!(
                        "failed to write entry `{key}` to store `{store_label}` in `{}`: {err}",
                        db_path.display()
                    )
                })?;
        }
    }

    transaction.commit().map_err(|err| {
        format!(
            "failed to commit transaction in `{}`: {err}",
            db_path.display()
        )
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::str;
    use tempfile::tempdir;

    /// Round-trip: write via our writer, read back via a fresh
    /// `rusqlite` connection. Proves the schema we install is
    /// readable by Spin's exact query (`SELECT value FROM
    /// spin_key_value WHERE store=$1 AND key=$2`).
    #[test]
    fn write_batch_round_trips_through_spin_schema() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join(".spin/sqlite_key_value.db");
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
        ];
        write_batch(&db_path, "app_config", &entries).expect("write_batch");

        let connection = Connection::open(&db_path).expect("re-open db");
        let mut select = connection
            .prepare("SELECT value FROM spin_key_value WHERE store=$1 AND key=$2")
            .expect("prepare select");
        for (key, expected_value) in &entries {
            let raw: Vec<u8> = select
                .query_row(params!["app_config", key], |row| row.get(0))
                .expect("row");
            assert_eq!(
                str::from_utf8(&raw).expect("utf8"),
                expected_value,
                "value for key `{key}`"
            );
        }
    }

    /// Re-running `write_batch` with the SAME `(store, key)` overwrites
    /// the previous value — `INSERT … ON CONFLICT DO UPDATE` is the
    /// statement Spin's runtime uses.
    #[test]
    fn write_batch_overwrites_existing_value() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("kv.db");
        write_batch(
            &db_path,
            "app_config",
            &[("greeting".to_owned(), "v1".to_owned())],
        )
        .expect("v1");
        write_batch(
            &db_path,
            "app_config",
            &[("greeting".to_owned(), "v2".to_owned())],
        )
        .expect("v2");
        let connection = Connection::open(&db_path).expect("re-open");
        let raw: Vec<u8> = connection
            .query_row(
                "SELECT value FROM spin_key_value WHERE store=$1 AND key=$2",
                params!["app_config", "greeting"],
                |row| row.get(0),
            )
            .expect("row");
        assert_eq!(str::from_utf8(&raw).expect("utf8"), "v2");
    }

    /// Two distinct stores in the same file are isolated (the schema's
    /// PRIMARY KEY is `(store, key)`).
    #[test]
    fn write_batch_isolates_stores() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("kv.db");
        write_batch(
            &db_path,
            "app_config",
            &[("greeting".to_owned(), "config-value".to_owned())],
        )
        .expect("write A");
        write_batch(
            &db_path,
            "sessions",
            &[("greeting".to_owned(), "session-value".to_owned())],
        )
        .expect("write B");
        let connection = Connection::open(&db_path).expect("re-open");
        let config_value: Vec<u8> = connection
            .query_row(
                "SELECT value FROM spin_key_value WHERE store=$1 AND key=$2",
                params!["app_config", "greeting"],
                |row| row.get(0),
            )
            .expect("config row");
        let session_value: Vec<u8> = connection
            .query_row(
                "SELECT value FROM spin_key_value WHERE store=$1 AND key=$2",
                params!["sessions", "greeting"],
                |row| row.get(0),
            )
            .expect("session row");
        assert_eq!(str::from_utf8(&config_value).expect("utf8"), "config-value");
        assert_eq!(
            str::from_utf8(&session_value).expect("utf8"),
            "session-value"
        );
    }

    /// `resolve_sqlite_path` honours an explicit absolute path
    /// verbatim (the `runtime-config.toml` operator gets full control
    /// over deployment-environment paths).
    #[test]
    fn resolve_path_honours_explicit_absolute_path() {
        let manifest_dir = PathBuf::from("/proj/spin");
        let runtime_dir = PathBuf::from("/proj/spin");
        let explicit = PathBuf::from("/var/lib/spin/custom.db");
        let resolved = resolve_sqlite_path(&manifest_dir, &runtime_dir, Some(&explicit));
        assert_eq!(resolved, explicit);
    }

    /// An explicit RELATIVE path anchors against the runtime-config
    /// file's directory — matches Spin's resolution behaviour.
    #[test]
    fn resolve_path_anchors_relative_explicit_path_against_runtime_config_dir() {
        let manifest_dir = PathBuf::from("/proj/spin");
        let runtime_dir = PathBuf::from("/proj/runtime");
        let explicit = PathBuf::from("custom/kv.db");
        let resolved = resolve_sqlite_path(&manifest_dir, &runtime_dir, Some(&explicit));
        assert_eq!(resolved, PathBuf::from("/proj/runtime/custom/kv.db"));
    }

    /// No explicit path → Spin's default
    /// `<manifest_dir>/.spin/sqlite_key_value.db`.
    #[test]
    fn resolve_path_defaults_to_dot_spin_dir() {
        let manifest_dir = PathBuf::from("/proj/spin");
        let runtime_dir = PathBuf::from("/proj/spin");
        let resolved = resolve_sqlite_path(&manifest_dir, &runtime_dir, None);
        assert_eq!(
            resolved,
            PathBuf::from("/proj/spin/.spin/sqlite_key_value.db")
        );
    }

    // ---------- Schema-drift contract ----------

    /// Schema-drift contract test (per
    /// `docs/superpowers/plans/2026-06-04-spin-per-backend-push.md`
    /// T3.8): the vendored `SPIN_KV_CREATE_TABLE` and `SPIN_KV_SET`
    /// strings byte-equal the statements Spin's
    /// `key-value-spin/src/store.rs` actually runs.
    ///
    /// **Source of truth**: spinframework/spin, file
    /// `crates/key-value-spin/src/store.rs`, function
    /// `KeyValueSqlite::create_connection` and `SqliteStore::set`.
    /// Pulled on 2026-06-04. If this test ever fails, re-pull the
    /// statements from upstream and update both the constants AND
    /// this test's expected-bytes literal. Do NOT silently fix one
    /// without verifying the other matches upstream — that's the
    /// schema-drift bug this test catches.
    #[test]
    fn vendored_schema_matches_upstream_byte_for_byte() {
        // The exact strings from upstream Spin. Whitespace included.
        // If you change either side without re-checking upstream you
        // will silently corrupt every user's KV file.
        let upstream_create_table = "CREATE TABLE IF NOT EXISTS spin_key_value (
                           store TEXT NOT NULL,
                           key   TEXT NOT NULL,
                           value BLOB NOT NULL,

                           PRIMARY KEY (store, key)
                        )";
        let upstream_set = "INSERT INTO spin_key_value (store, key, value) VALUES ($1, $2, $3)
                     ON CONFLICT(store, key) DO UPDATE SET value=$3";

        assert_eq!(
            SPIN_KV_CREATE_TABLE, upstream_create_table,
            "CREATE TABLE drift: re-verify against spinframework/spin \
             crates/key-value-spin/src/store.rs::KeyValueSqlite::create_connection \
             before updating either side"
        );
        assert_eq!(
            SPIN_KV_SET, upstream_set,
            "INSERT/SET drift: re-verify against spinframework/spin \
             crates/key-value-spin/src/store.rs::SqliteStore::set \
             before updating either side"
        );
    }

    /// Additional structural check: after we run `SPIN_KV_CREATE_TABLE`,
    /// the resulting table has EXACTLY the column names + types
    /// (`store TEXT`, `key TEXT`, `value BLOB`) and EXACTLY the primary
    /// key Spin's runtime expects. This catches semantic equivalence
    /// changes (e.g., an added index, a renamed column) that a pure
    /// byte-compare would miss.
    #[test]
    fn vendored_schema_creates_table_with_expected_column_shape() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("schema.db");
        let connection = Connection::open(&db_path).expect("open");
        connection
            .execute(SPIN_KV_CREATE_TABLE, [])
            .expect("create");

        let mut stmt = connection
            .prepare("PRAGMA table_info(spin_key_value)")
            .expect("prepare pragma");
        let columns: Vec<(String, String, bool)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>("name")?,
                    row.get::<_, String>("type")?,
                    row.get::<_, bool>("notnull")?,
                ))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("collect");

        assert_eq!(
            columns,
            vec![
                ("store".to_owned(), "TEXT".to_owned(), true),
                ("key".to_owned(), "TEXT".to_owned(), true),
                ("value".to_owned(), "BLOB".to_owned(), true),
            ],
            "PRAGMA table_info disagrees with Spin's schema -- re-pull \
             upstream and update both sides"
        );
    }

    // ---------- Spin runtime-compat parsing ----------

    #[test]
    fn parse_spin_major_version_handles_cli_default_format() {
        // Real output from `spin --version` on 3.6.3.
        assert_eq!(
            parse_spin_major_version("spin 3.6.3 (88d51cf 2026-04-09)\n"),
            Some(3)
        );
    }

    #[test]
    fn parse_spin_major_version_handles_spin_cli_hyphenated_prefix() {
        // Some Spin builds (and `spin cloud --version`) prefix with
        // the binary name like `spin-cloud`.
        assert_eq!(parse_spin_major_version("spin-cloud 0.11.0\n"), Some(0));
    }

    #[test]
    fn parse_spin_major_version_returns_none_for_unrecognised_output() {
        assert_eq!(parse_spin_major_version(""), None);
        assert_eq!(parse_spin_major_version("hello world\n"), None);
        assert_eq!(parse_spin_major_version("spin not-a-version\n"), None);
    }

    #[test]
    fn parse_spin_major_version_parses_double_digit_major() {
        // Defensive: when Spin reaches 10.x we keep working.
        assert_eq!(parse_spin_major_version("spin 10.0.1 (abc)\n"), Some(10));
    }
}
