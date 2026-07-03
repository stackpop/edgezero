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

use edgezero_adapter::registry::{AdapterPushContext, ReadConfigEntry, ResolvedStoreId};
use rusqlite::{params, Connection};

use super::push_cloud;
use super::runtime_config;

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
///
/// **Test-only early-return:** under `cfg!(test)` this function
/// exits before touching `PATH`. Without it, the `spin --version`
/// lookup would race `push_cloud`'s tests, which temporarily prepend
/// a fake `spin` binary to `$PATH` — the fake spin would record
/// the `--version` invocation into the cloud test's argv log,
/// corrupting that test's assertions. The check is best-effort
/// warning logic with no production semantic impact, and the parser
/// is unit-tested independently, so skipping it in tests doesn't
/// lose meaningful coverage. `cfg!(test)` resolves at compile time
/// to a constant the optimizer folds away in release builds, and
/// importantly it's evaluated against THIS crate's test target — so
/// the check still runs when `write_batch` is called from
/// production code or from downstream crates' tests (e.g.
/// `app-demo-cli`).
fn verify_spin_runtime_compat() {
    if cfg!(test) {
        return;
    }
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
    // proceeds — Spin is optional from the writer's perspective. The
    // function itself early-returns under `cfg!(test)` to avoid
    // racing `push_cloud`'s fake-spin tests; see its doc comment.
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

/// Read `[application].name` from `spin.toml`. Required by the
/// Fermyon Cloud writer to address KV stores via the app-scoped
/// label model (`--app <app> --label <label>`).
///
/// # Errors
/// Returns a human-readable error string if the file can't be
/// read, isn't valid TOML, or omits `[application].name`.
fn read_spin_application_name(spin_path: &Path) -> Result<String, String> {
    let raw = fs::read_to_string(spin_path).map_err(|err| {
        format!(
            "failed to read spin manifest at {}: {err}",
            spin_path.display()
        )
    })?;
    let parsed: toml::Value = toml::from_str(&raw)
        .map_err(|err| format!("failed to parse {} as TOML: {err}", spin_path.display()))?;
    parsed
        .as_table()
        .and_then(|root| root.get("application"))
        .and_then(toml::Value::as_table)
        .and_then(|app| app.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "spin manifest at {} is missing `[application].name`. Fermyon Cloud push needs the app name to address KV stores via `--app <name> --label <label>`. Add `[application]\\nname = \"<your-app>\"` to spin.toml.",
                spin_path.display()
            )
        })
}

/// Dispatch `config push --adapter spin` to the right per-backend
/// writer based on `runtime-config.toml` + the adapter's
/// `[adapters.spin.commands].deploy` command (auto-detect for Fermyon
/// Cloud).
///
/// Decision order:
/// 1. **`--local` is set**: force SQLite-direct against the local
///    `.spin/sqlite_key_value.db`. Fermyon Cloud auto-detect cannot
///    fire — even when the manifest's deploy command would otherwise
///    trip it. This lets the operator push into a local KV without
///    needing to authenticate with Fermyon Cloud first.
/// 2. **Deploy command auto-detects Fermyon Cloud** (`spin deploy` /
///    `spin cloud deploy`): shell out to `spin cloud key-value set`.
/// 3. **`runtime-config.toml` exists and declares this label's
///    backend**: dispatch on `type` — `spin` → SQLite-direct, `redis`
///    / `azure_cosmos` / `Unknown` → clear error pointing at the
///    backend's native CLI.
/// 4. **Default**: SQLite-direct at Spin's default path.
pub(super) fn dispatch_push(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    store: &ResolvedStoreId,
    entries: &[(String, String)],
    push_ctx: &AdapterPushContext<'_>,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let platform = store.platform.as_str();
    let logical = store.logical.as_str();

    if entries.is_empty() {
        return Ok(vec![format!(
            "no config entries to push to spin store `{platform}` (logical id `{logical}`)"
        )]);
    }

    let spin_manifest_path = adapter_manifest_path
        .map(|rel| manifest_root.join(rel))
        .ok_or_else(|| {
            "[adapters.spin.adapter].manifest must point at spin.toml for config push".to_owned()
        })?;
    let spin_manifest_dir = spin_manifest_path.parent().unwrap_or(manifest_root);

    // --runtime-config wins; otherwise default to
    // `runtime-config.toml` next to the spin manifest. Path math
    // only — the actual `runtime_config::read` is deferred until a
    // branch that NEEDS the parsed file, so a malformed/unreadable
    // `runtime-config.toml` doesn't block the cloud branch (which
    // only needs `spin.toml`'s `[application].name`).
    let runtime_config_path = push_ctx.runtime_config_path.map_or_else(
        || spin_manifest_dir.join("runtime-config.toml"),
        Path::to_path_buf,
    );
    let runtime_config_dir = runtime_config_path.parent().unwrap_or(spin_manifest_dir);

    // 1. `--local` forces SQLite-direct EVERY time. We skip both the
    //    Fermyon Cloud auto-detect AND the runtime-config backend
    //    dispatch — even if the operator's `runtime-config.toml`
    //    declares `type = "redis"` / `azure_cosmos` / unknown for
    //    this label. The intent of `--local` is "I want to seed my
    //    local dev loop, regardless of what the deployed app's
    //    backend selection looks like". An explicit `--runtime-config
    //    <path>` is still honoured for resolving the SQLite path,
    //    but the backend `type` is ignored.
    //
    //    We still enforce the Spin runtime invariant that any
    //    non-`default` label MUST be declared in `runtime-config.toml`
    //    — without it, `spin up` errors with "unknown
    //    key_value_stores label X" and the SQLite file we wrote is
    //    unreadable from the running app. See `verify_label_declared`.
    if push_ctx.local {
        let parsed = runtime_config::read(&runtime_config_path)?;
        verify_label_declared(platform, &parsed, &runtime_config_path)?;
        return write_sqlite(
            spin_manifest_dir,
            runtime_config_dir,
            // If the operator DID declare `type = "spin"` with an
            // explicit `path`, honour that path; otherwise fall
            // through to Spin's default `.spin/sqlite_key_value.db`.
            // Other backend types are silently treated as "no
            // explicit path" so SQLite-direct still happens.
            match parsed.key_value_stores.get(platform) {
                Some(runtime_config::KeyValueBackend::Spin { path }) => path.as_deref(),
                _ => None,
            },
            platform,
            logical,
            entries,
            dry_run,
        );
    }

    // 2. Else, if the manifest deploy command shells to `spin deploy`,
    //    treat as Fermyon Cloud. Cloud's `set` subcommand addresses
    //    the cloud KV store via Fermyon's app-scoped label model
    //    (`--app <app> --label <label>`), so we need the spin app
    //    name from spin.toml. We DO NOT read `runtime-config.toml`
    //    here — the cloud writer doesn't consult any local backend
    //    declaration, and parsing the file would gratuitously block
    //    cloud pushes (including `--dry-run` previews) on syntax
    //    errors in a file that doesn't influence the cloud path.
    if push_cloud::deploy_command_targets_fermyon_cloud(push_ctx.manifest_adapter_deploy_cmd) {
        let app_name = read_spin_application_name(&spin_manifest_path)?;
        if dry_run {
            // Run the same validation the real push runs: a `=` in
            // a key would be silently split by `spin`'s upstream
            // `KEY=VALUE` parser, and any single entry / cumulative
            // argv chunk over the safe-argv cap would fail the
            // shellout. Surfacing these in dry-run means a "clean"
            // preview is a real predictor of push success — without
            // it, the operator gets a green dry-run followed by a
            // hard failure on the actual push.
            let chunks = push_cloud::chunk_entries(entries)?;
            let mut out = Vec::with_capacity(entries.len().saturating_add(1));
            out.push(format!(
                "would shell `spin cloud key-value set --app {app_name} --label {platform} KEY=VALUE [...]` for {} entries across {} invocation(s) (logical id `{logical}`):",
                entries.len(),
                chunks.len()
            ));
            for (key, _) in entries {
                out.push(format!("  would set `{key}`"));
            }
            return Ok(out);
        }
        push_cloud::write_batch(&app_name, platform, entries)?;
        return Ok(vec![format!(
            "pushed {} entries to Fermyon Cloud KV store linked to app `{app_name}` label `{platform}` (logical id `{logical}`) via `spin cloud key-value set`",
            entries.len()
        )]);
    }

    // 3 / 4: SQLite-direct dispatch. Look up the backend explicitly
    // declared for this label, falling back to Spin's default
    // `(type = "spin", path = None)` if the label has no stanza.
    let parsed = runtime_config::read(&runtime_config_path)?;
    let backend = parsed.key_value_stores.get(platform);
    match backend {
        Some(runtime_config::KeyValueBackend::Redis { url }) => Err(format!(
            "store `{platform}` (logical id `{logical}`) is backed by `type = \"redis\"` (url: `{url}`) in {}; `config push --adapter spin` does not yet support the redis backend in this version. Use `redis-cli -u {url} SET <key> <value>` directly.",
            runtime_config_path.display()
        )),
        Some(runtime_config::KeyValueBackend::AzureCosmos) => Err(format!(
            "store `{platform}` (logical id `{logical}`) is backed by `type = \"azure_cosmos\"` in {}; `config push --adapter spin` does not yet support the Azure backend in this version. Use Azure's CosmosDB SDK / `az cosmosdb` CLI directly.",
            runtime_config_path.display()
        )),
        Some(runtime_config::KeyValueBackend::Unknown { type_name }) => Err(format!(
            "store `{platform}` (logical id `{logical}`) is backed by an unrecognised type `{type_name}` in {}; `config push --adapter spin` only supports `type = \"spin\"` for now. Use the backend's native CLI to seed entries directly.",
            runtime_config_path.display()
        )),
        Some(runtime_config::KeyValueBackend::Spin { path }) => write_sqlite(
            spin_manifest_dir,
            runtime_config_dir,
            path.as_deref(),
            platform,
            logical,
            entries,
            dry_run,
        ),
        None => {
            // Spin's runtime auto-provides ONLY the `default` label;
            // any other label must have a `[key_value_store.<label>]`
            // stanza or `spin up` errors. We fall through to SQLite-
            // direct only for `default`; non-default un-declared
            // labels error early so the operator doesn't write a
            // file the running app can't open.
            verify_label_declared(platform, &parsed, &runtime_config_path)?;
            write_sqlite(
                spin_manifest_dir,
                runtime_config_dir,
                None,
                platform,
                logical,
                entries,
                dry_run,
            )
        }
    }
}

/// Spin's runtime auto-provides ONLY the `default` KV label. Any
/// other label must be declared in `runtime-config.toml`; without
/// it `spin up` errors with `unknown key_value_stores label X` and
/// the `SQLite` file our writer just created is unreadable from
/// the running app. This helper enforces the same invariant at push
/// time so a silent "push succeeds, runtime can't open store"
/// divergence can't happen.
pub(super) fn verify_label_declared(
    platform: &str,
    parsed: &runtime_config::ParsedRuntimeConfig,
    runtime_config_path: &Path,
) -> Result<(), String> {
    if platform == "default" || parsed.key_value_stores.contains_key(platform) {
        return Ok(());
    }
    Err(format!(
        "label `{platform}` has no `[key_value_store.{platform}]` stanza in {}. Spin auto-provides ONLY the `default` label; any other label must be declared in runtime-config.toml or `spin up` errors with `unknown key_value_stores label {platform}`. Add `[key_value_store.{platform}]\\ntype = \"spin\"` (or the backend you want) to {} and retry.",
        runtime_config_path.display(),
        runtime_config_path.display(),
    ))
}

/// `SQLite`-direct write helper: resolves the `SQLite` path (honouring
/// any explicit `path` from `runtime-config.toml`), then either prints
/// a dry-run preview or actually writes the batch through
/// [`write_batch`].
fn write_sqlite(
    spin_manifest_dir: &Path,
    runtime_config_dir: &Path,
    explicit_path: Option<&Path>,
    platform: &str,
    logical: &str,
    entries: &[(String, String)],
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let db_path = resolve_sqlite_path(spin_manifest_dir, runtime_config_dir, explicit_path);
    if dry_run {
        let mut out = Vec::with_capacity(entries.len().saturating_add(1));
        out.push(format!(
            "would write {} entries to SQLite-backed Spin KV at `{}` for store `{platform}` (logical id `{logical}`):",
            entries.len(),
            db_path.display()
        ));
        for (key, _) in entries {
            out.push(format!("  would set `{key}`"));
        }
        return Ok(out);
    }
    write_batch(&db_path, platform, entries)?;
    Ok(vec![format!(
        "pushed {} entries to Spin SQLite KV at `{}` for store `{platform}` (logical id `{logical}`)",
        entries.len(),
        db_path.display()
    )])
}

/// `SQLite`-direct read helper: opens the Spin KV database at `db_path`
/// and queries `SELECT value FROM spin_key_value WHERE store=$1 AND key=$2`.
///
/// Returns:
/// - `MissingStore` if the database file does not exist (same semantic
///   as the write path creating it on first write).
/// - `MissingKey` if the row is absent.
/// - `Present(value)` on a hit (value decoded from UTF-8 BLOB).
pub(super) fn read_sqlite_entry(
    db_path: &Path,
    store: &str,
    key: &str,
) -> Result<ReadConfigEntry, String> {
    use rusqlite::OptionalExtension as _;

    if !db_path.exists() {
        return Ok(ReadConfigEntry::MissingStore);
    }
    let connection = Connection::open(db_path)
        .map_err(|err| format!("failed to open `{}`: {err}", db_path.display()))?;
    // Ensure the schema exists so opening a fresh (empty) file doesn't error.
    connection
        .execute(SPIN_KV_CREATE_TABLE, [])
        .map_err(|err| format!("failed to verify schema in `{}`: {err}", db_path.display()))?;
    let raw: Option<Vec<u8>> = connection
        .query_row(
            "SELECT value FROM spin_key_value WHERE store=$1 AND key=$2",
            params![store, key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| {
            format!(
                "failed to query `{}` for store `{store}` key `{key}`: {err}",
                db_path.display()
            )
        })?;
    match raw {
        None => Ok(ReadConfigEntry::MissingKey),
        Some(bytes) => {
            let value = String::from_utf8(bytes).map_err(|err| {
                format!(
                    "value for store `{store}` key `{key}` in `{}` is not valid UTF-8: {err}",
                    db_path.display()
                )
            })?;
            Ok(ReadConfigEntry::Present(value))
        }
    }
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

    // ---------- dispatch_push matrix ----------
    //
    // These tests exercise `dispatch_push` directly with fixture
    // `AdapterPushContext` / `runtime-config.toml` shapes. The
    // function is where all the business logic of the per-backend
    // redesign lives, so the matrix has to be tight: each branch
    // (--local override, Fermyon Cloud auto-detect, redis-error,
    // azure-error, unknown-error, default-SQLite, explicit-Spin-with-
    // path, empty entries) gets a named test.

    use super::super::SpinCliAdapter;
    use edgezero_adapter::registry::Adapter as _;

    const TEST_CONFIG_ID: &str = "app_config";

    fn write_minimal_spin_toml(dir: &Path) -> PathBuf {
        let path = dir.join("spin.toml");
        fs::write(
            &path,
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"a.wasm\"\n",
        )
        .expect("write spin.toml");
        path
    }

    fn entries_two() -> Vec<(String, String)> {
        vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("svc.timeout".to_owned(), "1500".to_owned()),
        ]
    }

    fn store(logical: &str, platform: &str) -> ResolvedStoreId {
        ResolvedStoreId::new(logical.to_owned(), platform.to_owned())
    }

    #[test]
    fn dispatch_push_empty_entries_returns_noop_message() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        let push_ctx = AdapterPushContext::new();
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &[],
            &push_ctx,
            false,
        )
        .expect("dispatch ok");
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("no config entries"),
            "no-op message: {out:?}"
        );
    }

    #[test]
    fn dispatch_push_local_forces_sqlite_even_when_runtime_config_declares_redis() {
        // F1 (blocker): `--local` MUST bypass runtime-config backend
        // dispatch. Without this test, the code that says "Redis: error
        // out" would silently fire even under --local.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"redis\"\nurl = \"redis://localhost\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new().with_local(true);
        let entries = entries_two();
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries,
            &push_ctx,
            true, // dry-run so the test doesn't actually touch disk
        )
        .expect("--local + redis must dispatch to SQLite, not error");
        assert!(
            out[0].contains("SQLite-backed Spin KV"),
            "--local must force the SQLite writer: {out:?}"
        );
        assert!(
            !out.iter().any(|line| line.contains("redis-cli")),
            "--local must NOT emit the redis-cli error: {out:?}"
        );
    }

    #[test]
    fn dispatch_push_local_forces_sqlite_even_when_runtime_config_declares_azure() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"azure_cosmos\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new().with_local(true);
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("--local + azure must dispatch to SQLite");
        assert!(out[0].contains("SQLite-backed Spin KV"), "{out:?}");
    }

    #[test]
    fn dispatch_push_local_forces_sqlite_even_when_deploy_targets_fermyon_cloud() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        // Non-default labels require a runtime-config stanza so
        // `spin up` can open them; with the stanza in place, --local
        // dispatches to SQLite regardless of the Fermyon Cloud deploy
        // command.
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new()
            .with_local(true)
            .with_manifest_adapter_deploy_cmd("spin deploy --from ./");
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("--local must beat Fermyon Cloud auto-detect");
        assert!(out[0].contains("SQLite-backed Spin KV"), "{out:?}");
        assert!(
            !out.iter()
                .any(|line| line.contains("spin cloud key-value set")),
            "Fermyon Cloud writer must NOT fire under --local: {out:?}"
        );
    }

    #[test]
    fn dispatch_push_local_honours_explicit_spin_path() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\npath = \"custom/kv.db\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new().with_local(true);
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("dispatch ok");
        let expected_path = dir.path().join("custom/kv.db");
        assert!(
            out.iter()
                .any(|line| line.contains(&expected_path.display().to_string())),
            "explicit path under --local: expected {} in {out:?}",
            expected_path.display()
        );
    }

    #[test]
    fn dispatch_push_fermyon_cloud_auto_detects_from_spin_deploy_and_uses_app_label_shape() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path()); // [application].name = "x"

        let push_ctx =
            AdapterPushContext::new().with_manifest_adapter_deploy_cmd("spin deploy --from ./");
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("dispatch ok");
        // The preview MUST show Fermyon's app-scoped label shape
        // (`--app <APP> --label <LABEL> KEY=VALUE`), not the
        // pre-fix `--store <STORE>` shape.
        assert!(
            out[0].contains("spin cloud key-value set"),
            "cloud writer dry-run preview: {out:?}"
        );
        assert!(
            out[0].contains("--app x") && out[0].contains("--label app_config"),
            "must use --app <spin app name> + --label <platform label> per Fermyon's app-scoped label model: {out:?}"
        );
        assert!(
            !out[0].contains("--store"),
            "must NOT use --store (conflates spin label with cloud KV resource name): {out:?}"
        );
    }

    #[test]
    fn dispatch_push_fermyon_cloud_dry_run_ignores_malformed_runtime_config() {
        // Cloud push (set / link) consults only spin.toml's
        // `[application].name` + the env-resolved label — it never
        // reads `runtime-config.toml`. So a malformed local runtime
        // config (someone mid-edit, a stale file from another
        // project, etc.) MUST NOT block cloud `--dry-run` previews.
        // Before this fix `dispatch_push` parsed runtime-config
        // unconditionally at the top, so any TOML syntax error
        // surfaced before the cloud branch even ran.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path()); // [application].name = "x"
        fs::write(
            dir.path().join("runtime-config.toml"),
            "this is not [valid toml at all\n",
        )
        .expect("write malformed runtime-config");

        let push_ctx =
            AdapterPushContext::new().with_manifest_adapter_deploy_cmd("spin deploy --from ./");
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("cloud dry-run must succeed despite malformed runtime-config");
        assert!(
            out[0].contains("spin cloud key-value set")
                && out[0].contains("--app x")
                && out[0].contains("--label app_config"),
            "cloud preview should render the app-scoped shape: {out:?}"
        );
    }

    #[test]
    fn dispatch_push_fermyon_cloud_dry_run_rejects_equals_in_key() {
        // The real push errors on a `=` in a key (would be silently
        // truncated by Spin's upstream `KEY=VALUE` parser). The
        // dry-run preview MUST surface the same error — otherwise an
        // operator gets a green dry-run followed by a hard failure on
        // the actual push.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        let bad = vec![("svc=timeout".to_owned(), "1500".to_owned())];

        let push_ctx =
            AdapterPushContext::new().with_manifest_adapter_deploy_cmd("spin deploy --from ./");
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &bad,
            &push_ctx,
            true,
        )
        .expect_err("dry-run must reject `=` in keys");
        assert!(
            err.contains("contains `=`"),
            "dry-run error must surface the same KEY=VALUE diagnostic the real push gives: {err}"
        );
    }

    #[test]
    fn dispatch_push_fermyon_cloud_errors_when_spin_application_name_missing() {
        // The cloud writer needs `[application].name` for `--app`.
        // A spin.toml without it must error with an actionable
        // message, not silently shell `spin cloud key-value set
        // --app  --label …` (which would fail upstream with an
        // unhelpful clap error).
        let dir = tempdir().expect("tempdir");
        // Note: NO `[application]` section.
        let spin_path = dir.path().join("spin.toml");
        fs::write(
            &spin_path,
            "spin_manifest_version = 2\n[component.demo]\nsource = \"a.wasm\"\n",
        )
        .expect("write spin.toml");

        let push_ctx =
            AdapterPushContext::new().with_manifest_adapter_deploy_cmd("spin deploy --from ./");
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("missing [application].name must error");
        assert!(
            err.contains("[application].name") && err.contains("spin.toml"),
            "error must name the missing field + the file: {err}"
        );
    }

    #[test]
    fn dispatch_push_redis_backend_errors_with_redis_cli_hint() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"redis\"\nurl = \"redis://localhost:6379\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new();
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("redis backend without --local must error");
        assert!(
            err.contains("redis-cli") && err.contains("redis://localhost:6379"),
            "redis error must name the cli + url: {err}"
        );
    }

    #[test]
    fn dispatch_push_azure_backend_errors_with_azure_cli_hint() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"azure_cosmos\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new();
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("azure backend without --local must error");
        assert!(
            err.contains("az cosmosdb"),
            "azure error must name the cli: {err}"
        );
    }

    #[test]
    fn dispatch_push_unknown_backend_errors_with_type_name() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"someones-new-backend\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new();
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("unknown backend must error");
        assert!(
            err.contains("someones-new-backend"),
            "unknown-type error must name the type: {err}"
        );
    }

    #[test]
    fn dispatch_push_default_label_with_no_runtime_config_dispatches_to_sqlite() {
        // Spin auto-provides ONLY the `default` label. With no
        // runtime-config.toml present and the platform label set to
        // `default`, we fall through to the SQLite writer.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());

        let push_ctx = AdapterPushContext::new();
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("default", "default"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("`default` label -> SQLite");
        let expected = dir.path().join(".spin/sqlite_key_value.db");
        assert!(
            out.iter()
                .any(|line| line.contains(&expected.display().to_string())),
            "default SQLite path: expected {} in {out:?}",
            expected.display()
        );
    }

    #[test]
    fn dispatch_push_non_default_label_without_runtime_config_stanza_errors() {
        // H2: Spin's runtime can't open a custom label without a
        // `[key_value_store.<label>]` entry. Silently writing SQLite
        // for `app_config` when the operator hasn't declared it is
        // worse than erroring -- the push "succeeds" but the running
        // app fails at `Store::open("app_config")`. Catch this at
        // push time so the operator gets an actionable hint.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());

        let push_ctx = AdapterPushContext::new();
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("non-default label without runtime-config stanza must error");
        assert!(
            err.contains("`[key_value_store.app_config]`")
                && err.contains("unknown key_value_stores label app_config")
                && err.contains("type = \"spin\""),
            "error must name the stanza, the runtime symptom, AND the fix: {err}"
        );
    }

    #[test]
    fn dispatch_push_non_default_label_with_runtime_config_stanza_dispatches_to_sqlite() {
        // Counterpart to the test above: with the stanza in place,
        // the same `app_config` dispatch succeeds.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new();
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("non-default label WITH stanza must dispatch");
        assert!(out[0].contains("SQLite-backed Spin KV"), "{out:?}");
    }

    #[test]
    fn dispatch_push_custom_runtime_config_path_is_honoured() {
        // H3 from the test-coverage review: prove --runtime-config
        // <path> is actually read from the non-default location.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        let custom = dir.path().join("alternate-runtime.toml");
        fs::write(
            &custom,
            "[key_value_store.app_config]\ntype = \"redis\"\nurl = \"redis://elsewhere\"\n",
        )
        .expect("write");

        let push_ctx = AdapterPushContext::new().with_runtime_config_path(&custom);
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("custom runtime-config's redis declaration must fire");
        assert!(
            err.contains("redis://elsewhere"),
            "custom runtime-config was read: {err}"
        );
    }

    #[test]
    fn dispatch_push_unrelated_label_in_runtime_config_does_not_affect_dispatch() {
        // Sanity: only the matching label's stanza is consulted. A
        // [key_value_store.other_label] redis entry must NOT prevent
        // a SQLite-direct push to app_config (which has its own
        // declared stanza).
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n\n\
             [key_value_store.other_label]\ntype = \"redis\"\nurl = \"redis://nowhere\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new();
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("unrelated label must not block dispatch");
        assert!(out[0].contains("SQLite-backed Spin KV"), "{out:?}");
    }

    // ---------- read_config_entry / read_config_entry_local ----------

    // Helper: seed a key into the SQLite DB at `db_path` for `store_label`.
    fn write_kv_entry(db_path: &Path, store_label: &str, key: &str, value: &str) {
        write_batch(db_path, store_label, &[(key.to_owned(), value.to_owned())])
            .expect("seed entry");
    }

    // Branch 3a: redis backend → error naming the backend.
    #[test]
    fn read_config_entry_errors_for_redis_backend() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"redis\"\nurl = \"redis://localhost:6379\"\n",
        )
        .expect("write runtime-config");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter.read_config_entry(
            dir.path(),
            Some("spin.toml"),
            None,
            &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
            "greeting",
            &ctx,
        );
        match result {
            Err(err) => assert!(
                err.contains("redis") && err.contains("app_config"),
                "error names the backend and store: {err}"
            ),
            Ok(_) => panic!("expected Err for redis backend"),
        }
    }

    // Branch 3b: azure_cosmos backend → error naming the backend.
    #[test]
    fn read_config_entry_errors_for_azure_cosmos_backend() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"azure_cosmos\"\n",
        )
        .expect("write runtime-config");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter.read_config_entry(
            dir.path(),
            Some("spin.toml"),
            None,
            &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
            "greeting",
            &ctx,
        );
        match result {
            Err(err) => assert!(
                err.contains("azure_cosmos") && err.contains("app_config"),
                "error names the backend and store: {err}"
            ),
            Ok(_) => panic!("expected Err for azure_cosmos backend"),
        }
    }

    // Branch 3c: unknown backend → error naming the type.
    #[test]
    fn read_config_entry_errors_for_unknown_backend() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"future-backend\"\n",
        )
        .expect("write runtime-config");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter.read_config_entry(
            dir.path(),
            Some("spin.toml"),
            None,
            &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
            "greeting",
            &ctx,
        );
        match result {
            Err(err) => assert!(
                err.contains("future-backend"),
                "error names the unrecognised type: {err}"
            ),
            Ok(_) => panic!("expected Err for unknown backend"),
        }
    }

    // Branch 4: default `type = "spin"` → SQLite-direct, Present.
    #[test]
    fn read_config_entry_returns_present_for_spin_backend() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");
        let db_path = dir.path().join(".spin/sqlite_key_value.db");
        write_kv_entry(&db_path, "app_config", "greeting", "hello-spin");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "greeting",
                &ctx,
            )
            .expect("spin backend read ok");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "hello-spin");
    }

    // Branch 4: default label (no runtime-config stanza) → MissingStore
    // when the database file doesn't exist.
    #[test]
    fn read_config_entry_returns_missing_store_when_db_absent() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        // No runtime-config.toml → default label rules apply.
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store("default", "default"),
                "greeting",
                &ctx,
            )
            .expect("missing db is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "absent SQLite file must yield MissingStore"
        );
    }

    // Branch 4: key absent in an existing DB → MissingKey.
    #[test]
    fn read_config_entry_returns_missing_key_when_key_absent() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        let db_path = dir.path().join(".spin/sqlite_key_value.db");
        write_kv_entry(&db_path, "default", "other_key", "v");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store("default", "default"),
                "greeting",
                &ctx,
            )
            .expect("missing key is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "absent key must yield MissingKey"
        );
    }

    // Malformed runtime-config.toml propagates as an error
    // (not silently swallowed with .ok()).
    #[test]
    fn read_config_entry_propagates_malformed_runtime_config_error() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config\ntype = \"spin\"\n", // missing closing `]`
        )
        .expect("write bad runtime-config");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter.read_config_entry(
            dir.path(),
            Some("spin.toml"),
            None,
            &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
            "greeting",
            &ctx,
        );
        match result {
            Err(err) => assert!(
                err.contains("failed to parse") || err.contains("runtime-config"),
                "error names the parse failure: {err}"
            ),
            Ok(_) => panic!("expected Err for malformed runtime-config"),
        }
    }

    // Branch 1: --local forces SQLite-direct regardless of backend type.
    #[test]
    fn read_config_entry_local_reads_sqlite_ignoring_backend_type() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        // Declare a spin backend so label is declared for verify_label_declared.
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");
        let db_path = dir.path().join(".spin/sqlite_key_value.db");
        write_kv_entry(&db_path, "app_config", "mode", "local");
        let mut ctx = AdapterPushContext::new();
        ctx.local = true;
        let result = SpinCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "mode",
                &ctx,
            )
            .expect("local read ok");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "local");
    }

    // Branch 1 via read_config_entry: push_ctx.local=true delegates to local impl.
    #[test]
    fn read_config_entry_with_local_flag_delegates_to_local_impl() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");
        let db_path = dir.path().join(".spin/sqlite_key_value.db");
        write_kv_entry(&db_path, "app_config", "flag", "set");
        let mut ctx = AdapterPushContext::new();
        ctx.local = true;
        // Even though Fermyon Cloud auto-detect would fire via deploy_cmd,
        // local flag must win.
        ctx.manifest_adapter_deploy_cmd = Some("spin deploy");
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "flag",
                &ctx,
            )
            .expect("local flag wins over cloud detect");
        assert!(
            matches!(result, ReadConfigEntry::Present(_)),
            "local flag + cloud cmd must yield Present (SQLite wins)"
        );
    }

    // SQLite path anchors against runtime-config dir, not
    // spin manifest dir, for relative explicit paths.
    #[test]
    fn read_config_entry_sqlite_path_anchors_against_runtime_config_dir() {
        let dir = tempdir().expect("tempdir");
        // spin.toml at <tmp>/spin/spin.toml
        let spin_dir = dir.path().join("spin");
        fs::create_dir_all(&spin_dir).expect("create spin dir");
        fs::write(
            spin_dir.join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        )
        .expect("write spin.toml");
        // runtime-config.toml at <tmp>/cfg/runtime-config.toml with
        // path = "session.db" (relative).
        let cfg_dir = dir.path().join("cfg");
        fs::create_dir_all(&cfg_dir).expect("create cfg dir");
        let runtime_config_path = cfg_dir.join("runtime-config.toml");
        fs::write(
            &runtime_config_path,
            "[key_value_store.app_config]\ntype = \"spin\"\npath = \"session.db\"\n",
        )
        .expect("write runtime-config");
        // Seed the SQLite file at <tmp>/cfg/session.db (NOT spin/session.db).
        let db_path = cfg_dir.join("session.db");
        write_kv_entry(&db_path, "app_config", "key1", "val1");
        let mut ctx = AdapterPushContext::new();
        ctx.runtime_config_path = Some(&runtime_config_path);
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin/spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "key1",
                &ctx,
            )
            .expect("cfg-dir-anchored path read ok");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "val1", "value from cfg-dir-anchored db");
    }

    // Default path (no explicit path) falls back to spin manifest dir.
    #[test]
    fn read_config_entry_sqlite_default_path_anchors_against_spin_manifest_dir() {
        let dir = tempdir().expect("tempdir");
        let spin_dir = dir.path().join("spin");
        fs::create_dir_all(&spin_dir).expect("create spin dir");
        fs::write(
            spin_dir.join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        )
        .expect("write spin.toml");
        let cfg_dir = dir.path().join("cfg");
        fs::create_dir_all(&cfg_dir).expect("create cfg dir");
        let runtime_config_path = cfg_dir.join("runtime-config.toml");
        // No `path` in the stanza → default .spin/sqlite_key_value.db.
        fs::write(
            &runtime_config_path,
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");
        // Seed at <tmp>/spin/.spin/sqlite_key_value.db.
        let db_path = spin_dir.join(".spin/sqlite_key_value.db");
        write_kv_entry(&db_path, "app_config", "key2", "val2");
        let mut ctx = AdapterPushContext::new();
        ctx.runtime_config_path = Some(&runtime_config_path);
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin/spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "key2",
                &ctx,
            )
            .expect("default path read ok");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "val2", "value from spin-manifest-dir default db");
    }

    // Absolute path is honoured verbatim.
    #[test]
    fn read_config_entry_sqlite_absolute_path_honoured() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        // Use a tempfile as the target database with an absolute path.
        let db_file = tempfile::NamedTempFile::new().expect("tempfile");
        let db_path = db_file.path().to_path_buf();
        write_kv_entry(&db_path, "default", "abs_key", "abs_val");
        let abs_path_str = db_path.to_str().expect("abs path utf8").to_owned();
        // Write runtime-config with the absolute path.
        fs::write(
            dir.path().join("runtime-config.toml"),
            format!("[key_value_store.default]\ntype = \"spin\"\npath = \"{abs_path_str}\"\n"),
        )
        .expect("write runtime-config");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store("default", "default"),
                "abs_key",
                &ctx,
            )
            .expect("absolute path read ok");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "abs_val");
    }
}
