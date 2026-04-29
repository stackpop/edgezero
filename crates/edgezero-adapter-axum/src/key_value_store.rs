//! Persistent KV store for local development and testing.
//!
//! Values are stored in a `redb` embedded database with TTL support.
//! Data persists across server restarts, providing parity with edge provider
//! KV stores (Cloudflare Workers KV, Fastly KV Store).
//!
//! ## Storage Location
//!
//! By default, the development server stores data at `.edgezero/kv.redb`
//! in your project directory. Custom store names get their own derived
//! database file under `.edgezero/`. Add this path to your `.gitignore`:
//!
//! ```gitignore
//! .edgezero/
//! ```
//!
//! ## TTL and Cleanup
//!
//! Expired entries are lazily evicted when accessed via `get_bytes`.
//! Entries that are never accessed after expiration will remain in the
//! database until explicitly deleted.
//!
//! ## Database File Management
//!
//! The redb database file will grow over time and does not automatically
//! shrink after deletions. For development, this is typically not an issue.
//! To reclaim space, delete the corresponding file in `.edgezero/`
//! (data will be lost).
//!
//! ## Concurrent Access
//!
//! The database uses exclusive file locking. Only one process can access
//! a database file at a time. If you need to run multiple dev servers
//! simultaneously, use different database paths (e.g., by running them
//! in separate project directories).
//!
//! Within a single process, the store is thread-safe and supports
//! concurrent access via redb's transaction system.
//!
//! ## Performance Notes
//!
//! - All operations are ACID-compliant via redb's transaction system.
//! - The database file path acts as the namespace identifier, similar to
//!   how Cloudflare uses bindings and Fastly uses store names.

use std::ops::Bound;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::key_value_store::{KvError, KvPage, KvStore};
use redb::{Database, ReadableDatabase as _, ReadableTable as _, TableDefinition};
use std::time::SystemTime;

/// Table definition for the KV store.
/// Key: `String`, Value: `(Bytes, Option<expiration_timestamp_millis>)`
const KV_TABLE: TableDefinition<&str, (&[u8], Option<u128>)> = TableDefinition::new("kv");

/// Type alias for a writable KV table handle.
type KvTable<'txn> = redb::Table<'txn, &'static str, (&'static [u8], Option<u128>)>;

/// A persistent KV store backed by `redb`.
///
/// Suitable for local development where data persistence across restarts is needed.
/// TTL-expired entries are lazily evicted (checked on read/list).
pub struct PersistentKvStore {
    db: Database,
}

impl PersistentKvStore {
    const LIST_SCAN_BATCH_SIZE: usize = 256;
    /// Maximum number of scan batches before returning a partial page.
    ///
    /// Each batch scans up to `LIST_SCAN_BATCH_SIZE` entries, so this caps
    /// a single `list_keys_page` call at scanning ~25,600 entries regardless
    /// of how many are expired. Without this guard, a database that has
    /// accumulated large numbers of expired entries (common in long-running
    /// dev sessions) can produce unbounded scan latency.
    ///
    /// When the limit is hit the partial page is returned with the last
    /// live cursor, so callers can resume pagination normally on the next
    /// call. A warning is logged once so operators know cleanup is needed.
    const MAX_SCAN_BATCHES: usize = 100;

    fn begin_write(&self) -> Result<redb::WriteTransaction, KvError> {
        self.db
            .begin_write()
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to begin write txn: {err}")))
    }

    fn cleanup_expired_keys(&self, expired_keys: &[String]) -> Result<(), KvError> {
        if expired_keys.is_empty() {
            return Ok(());
        }

        let write_txn = self.begin_write()?;
        {
            let mut table = Self::open_table(&write_txn)?;
            for key in expired_keys {
                let still_expired = table
                    .get(key.as_str())
                    .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to get key: {err}")))?
                    .is_some_and(|entry| {
                        let (_, expires_at) = entry.value();
                        Self::is_expired(expires_at)
                    });
                if still_expired {
                    table.remove(key.as_str()).map_err(|err| {
                        KvError::Internal(anyhow::anyhow!("failed to remove: {err}"))
                    })?;
                }
            }
        }
        Self::commit(write_txn)
    }

    fn commit(txn: redb::WriteTransaction) -> Result<(), KvError> {
        txn.commit()
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to commit: {err}")))
    }

    /// Check if an entry is expired based on its expiration timestamp.
    ///
    /// If the system clock is before UNIX epoch (highly unlikely), treats entries
    /// as not expired to avoid incorrectly deleting data.
    fn is_expired(expires_at_millis: Option<u128>) -> bool {
        if let Some(exp) = expires_at_millis {
            match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                Ok(now) => now.as_millis() >= exp,
                Err(_) => {
                    // System clock is before UNIX epoch - treat as not expired
                    // to avoid incorrectly deleting data
                    false
                }
            }
        } else {
            false
        }
    }

    /// Create a new persistent KV store at the given path.
    ///
    /// # Behavior
    ///
    /// - If the file does not exist, a new database will be initialized
    /// - If the file exists and is a valid redb database, it will be opened with existing data preserved
    /// - If the file exists but is not a valid redb database, returns an error
    ///
    /// # Errors
    /// Returns an error if the database file cannot be opened or initialised (corrupted file, locked by another process, or insufficient permissions).
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, KvError> {
        let db_path = path.as_ref().display().to_string();
        let db = Database::create(path).map_err(|err| {
            KvError::Internal(anyhow::anyhow!(
                "Failed to open KV database at {db_path}. If the file is corrupted or locked \
                 by another process, try deleting it and restarting: {err}"
            ))
        })?;

        // Initialize the table
        let store = Self { db };
        let write_txn = store.begin_write()?;
        {
            let _table = Self::open_table(&write_txn)?;
        }
        Self::commit(write_txn)?;

        Ok(store)
    }

    fn open_table(txn: &redb::WriteTransaction) -> Result<KvTable<'_>, KvError> {
        txn.open_table(KV_TABLE)
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to open table: {err}")))
    }

    /// Convert `SystemTime` to milliseconds since UNIX epoch.
    ///
    /// Returns 0 if the time is before UNIX epoch (should never happen in practice).
    fn system_time_to_millis(time: SystemTime) -> u128 {
        time.duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0)
    }
}

#[async_trait(?Send)]
impl KvStore for PersistentKvStore {
    async fn delete(&self, key: &str) -> Result<(), KvError> {
        let write_txn = self.begin_write()?;
        let mut table = Self::open_table(&write_txn)?;
        table
            .remove(key)
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to remove: {err}")))?;
        drop(table);
        Self::commit(write_txn)
    }

    async fn exists(&self, key: &str) -> Result<bool, KvError> {
        Ok(self.get_bytes(key).await?.is_some())
    }

    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to begin read txn: {err}")))?;

        let table = read_txn
            .open_table(KV_TABLE)
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to open table: {err}")))?;

        if let Some(entry) = table
            .get(key)
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to get key: {err}")))?
        {
            let (value_bytes, expires_at) = entry.value();

            // Check if expired
            if Self::is_expired(expires_at) {
                // Drop read transaction before write
                drop(table);
                drop(read_txn);

                // Delete the expired key
                let write_txn = self.begin_write()?;
                {
                    let mut write_table = Self::open_table(&write_txn)?;
                    // Re-check expiry inside write txn to avoid TOCTOU race:
                    // a concurrent put_bytes may have overwritten the key with
                    // a fresh value between our read and this write.
                    let still_expired = write_table
                        .get(key)
                        .map_err(|err| {
                            KvError::Internal(anyhow::anyhow!("failed to get key: {err}"))
                        })?
                        .is_some_and(|fresh_entry| {
                            let (_, exp) = fresh_entry.value();
                            Self::is_expired(exp)
                        });
                    if still_expired {
                        write_table.remove(key).map_err(|err| {
                            KvError::Internal(anyhow::anyhow!("failed to remove: {err}"))
                        })?;
                    }
                }
                Self::commit(write_txn)?;

                return Ok(None);
            }

            Ok(Some(Bytes::copy_from_slice(value_bytes)))
        } else {
            Ok(None)
        }
    }

    async fn list_keys_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<KvPage, KvError> {
        let mut live_keys = Vec::with_capacity(limit.saturating_add(1));
        let mut scan_cursor = cursor.map(str::to_string);
        let mut reached_end = false;
        let mut batch_count: usize = 0;

        while live_keys.len() < limit.saturating_add(1) && !reached_end {
            if batch_count >= Self::MAX_SCAN_BATCHES {
                log::warn!(
                    "list_keys_page: scanned {} batches ({} entries) without filling the \
                     requested page; the database likely contains a large number of expired \
                     entries. Returning partial page. Run a KV cleanup to improve performance.",
                    Self::MAX_SCAN_BATCHES,
                    Self::MAX_SCAN_BATCHES.saturating_mul(Self::LIST_SCAN_BATCH_SIZE),
                );
                break;
            }
            batch_count = batch_count.saturating_add(1);
            let mut expired_keys = Vec::new();

            {
                let read_txn = self.db.begin_read().map_err(|err| {
                    KvError::Internal(anyhow::anyhow!("failed to begin read txn: {err}"))
                })?;

                let table = read_txn.open_table(KV_TABLE).map_err(|err| {
                    KvError::Internal(anyhow::anyhow!("failed to open table: {err}"))
                })?;

                let mut iter = if prefix.is_empty() {
                    match scan_cursor.as_deref() {
                        Some(scan_from) => {
                            table.range::<&str>((Bound::Excluded(scan_from), Bound::Unbounded))
                        }
                        None => table.iter(),
                    }
                } else {
                    match scan_cursor.as_deref() {
                        Some(scan_from) if scan_from >= prefix => {
                            table.range::<&str>((Bound::Excluded(scan_from), Bound::Unbounded))
                        }
                        _ => table.range(prefix..),
                    }
                }
                .map_err(|err| {
                    KvError::Internal(anyhow::anyhow!("failed to create range: {err}"))
                })?;

                for _ in 0..Self::LIST_SCAN_BATCH_SIZE {
                    let Some(entry) = iter.next() else {
                        reached_end = true;
                        break;
                    };

                    let (key_handle, value) = entry.map_err(|err| {
                        KvError::Internal(anyhow::anyhow!("failed to read range entry: {err}"))
                    })?;
                    let key = key_handle.value().to_owned();

                    if !prefix.is_empty() && !key.starts_with(prefix) {
                        reached_end = true;
                        break;
                    }

                    scan_cursor = Some(key.clone());
                    let (_, expires_at) = value.value();

                    if Self::is_expired(expires_at) {
                        expired_keys.push(key);
                        continue;
                    }

                    live_keys.push(key);
                    if live_keys.len() == limit.saturating_add(1) {
                        break;
                    }
                }
            }

            self.cleanup_expired_keys(&expired_keys)?;
        }

        let has_more = live_keys.len() > limit;
        if has_more {
            live_keys.truncate(limit);
        }

        Ok(KvPage {
            cursor: has_more.then(|| live_keys.last().cloned()).flatten(),
            keys: live_keys,
        })
    }

    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        let write_txn = self.begin_write()?;
        let mut table = Self::open_table(&write_txn)?;
        table
            .insert(key, (value.as_ref(), None))
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to insert: {err}")))?;
        drop(table);
        Self::commit(write_txn)
    }

    async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        ttl: Duration,
    ) -> Result<(), KvError> {
        let expires_at = SystemTime::now()
            .checked_add(ttl)
            .ok_or_else(|| KvError::Internal(anyhow::anyhow!("ttl overflows system time")))?;
        let expires_at_millis = Self::system_time_to_millis(expires_at);

        let write_txn = self.begin_write()?;
        let mut table = Self::open_table(&write_txn)?;
        table
            .insert(key, (value.as_ref(), Some(expires_at_millis)))
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to insert: {err}")))?;
        drop(table);
        Self::commit(write_txn)
    }
}

#[cfg(test)]
mod tests {
    // Run the shared contract tests against PersistentKvStore.
    // `Box::leak` intentionally extends the TempDir's lifetime to 'static so
    // it remains alive for the duration of the test process. The directory is
    // deleted when the process exits, unlike `.keep()` which leaves it behind
    // permanently.
    edgezero_core::key_value_store_contract_tests!(persistent_kv_contract, {
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let db_path = dir.path().join("contract.redb");
        PersistentKvStore::new(db_path).unwrap()
    });

    use super::*;
    use edgezero_core::key_value_store::KvHandle;
    use futures::executor;
    use std::sync::Arc;
    use std::thread;

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Config {
        enabled: bool,
        name: String,
    }

    fn store() -> (KvHandle, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let store = PersistentKvStore::new(db_path).unwrap();
        (KvHandle::new(Arc::new(store)), temp_dir)
    }

    #[tokio::test]
    async fn cleanup_expired_keys_does_not_delete_fresh_overwrite() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let kv_store = PersistentKvStore::new(db_path).unwrap();

        kv_store
            .put_bytes_with_ttl("race/key", Bytes::from("stale"), Duration::from_millis(1))
            .await
            .unwrap();
        thread::sleep(Duration::from_millis(200));
        kv_store
            .put_bytes("race/key", Bytes::from("fresh"))
            .await
            .unwrap();

        kv_store
            .cleanup_expired_keys(&["race/key".to_owned()])
            .unwrap();

        assert_eq!(
            kv_store.get_bytes("race/key").await.unwrap(),
            Some(Bytes::from("fresh"))
        );
    }

    #[test]
    fn concurrent_writes_dont_panic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let kv_store = PersistentKvStore::new(db_path).unwrap();
        let handle = KvHandle::new(Arc::new(kv_store));

        // KvHandle futures are !Send (async_trait(?Send) for WASM compat), so
        // tokio::spawn is off-limits. Use OS threads instead — KvHandle is
        // Send + Sync, so each thread moves its own clone and runs its own
        // executor. This is genuinely concurrent at the OS level.
        let threads: Vec<_> = (0_i32..100_i32)
            .map(|idx| {
                let kv_handle = handle.clone();
                thread::spawn(move || {
                    executor::block_on(async move {
                        let key = format!("key:{idx}");
                        kv_handle.put(&key, &idx).await.unwrap();
                    });
                })
            })
            .collect();

        for thread in threads {
            thread.join().expect("writer thread panicked");
        }

        // Verify all 100 keys survived concurrent writes with correct values.
        executor::block_on(async {
            for idx in 0_i32..100_i32 {
                let key = format!("key:{idx}");
                let val: i32 = handle.get_or(&key, -1_i32).await.unwrap();
                assert_eq!(
                    val, idx,
                    "key:{idx} has wrong value after concurrent writes"
                );
            }
        });
    }

    #[tokio::test]
    async fn data_persists_across_reopens() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        // Write data
        let store = PersistentKvStore::new(&db_path).unwrap();
        store
            .put_bytes("persistent", Bytes::from("value"))
            .await
            .unwrap();
        drop(store);

        // Reopen and verify data persists
        {
            let reopened = PersistentKvStore::new(&db_path).unwrap();
            let value = reopened.get_bytes("persistent").await.unwrap();
            assert_eq!(value, Some(Bytes::from("value")));
        }
    }

    #[tokio::test]
    async fn delete_nonexistent_is_ok() {
        let (kv_store, _dir) = store();
        kv_store.delete("nope").await.unwrap();
    }

    #[tokio::test]
    async fn delete_removes_key() {
        let (kv_store, _dir) = store();
        kv_store.put_bytes("k", Bytes::from("v")).await.unwrap();
        kv_store.delete("k").await.unwrap();
        assert_eq!(kv_store.get_bytes("k").await.unwrap(), None);
    }

    #[tokio::test]
    async fn exists_helper() {
        let (kv_store, _dir) = store();
        assert!(!kv_store.exists("nope").await.unwrap());
        kv_store.put_bytes("k", Bytes::from("v")).await.unwrap();
        assert!(kv_store.exists("k").await.unwrap());
    }

    #[tokio::test]
    async fn get_missing_key_returns_none() {
        let (kv_store, _dir) = store();
        assert_eq!(kv_store.get_bytes("missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn list_keys_page_skips_expired_entries() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let kv_store = PersistentKvStore::new(db_path).unwrap();

        kv_store
            .put_bytes("app/live", Bytes::from("value"))
            .await
            .unwrap();
        kv_store
            .put_bytes_with_ttl("app/expired", Bytes::from("gone"), Duration::from_millis(1))
            .await
            .unwrap();

        thread::sleep(Duration::from_millis(200));

        let page = kv_store.list_keys_page("app/", None, 10).await.unwrap();
        assert_eq!(page.keys, vec!["app/live".to_owned()]);
        assert_eq!(page.cursor, None);
    }

    #[tokio::test]
    async fn new_store_is_empty() {
        let (kv_store, _dir) = store();
        assert!(!kv_store.exists("anything").await.unwrap());
    }

    #[tokio::test]
    async fn put_and_get_bytes() {
        let (kv_store, _dir) = store();
        kv_store.put_bytes("k", Bytes::from("hello")).await.unwrap();
        assert_eq!(
            kv_store.get_bytes("k").await.unwrap(),
            Some(Bytes::from("hello"))
        );
    }

    #[tokio::test]
    async fn put_overwrites_existing() {
        let (kv_store, _dir) = store();
        kv_store.put_bytes("k", Bytes::from("first")).await.unwrap();
        kv_store
            .put_bytes("k", Bytes::from("second"))
            .await
            .unwrap();
        assert_eq!(
            kv_store.get_bytes("k").await.unwrap(),
            Some(Bytes::from("second"))
        );
    }

    #[tokio::test]
    async fn ttl_expires_entry() {
        // Use the store impl directly to bypass validation limits (min TTL 60s)
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let kv_store = PersistentKvStore::new(db_path).unwrap();
        kv_store
            .put_bytes_with_ttl("temp", Bytes::from("val"), Duration::from_millis(1))
            .await
            .unwrap();
        // 200ms gives the OS scheduler enough headroom on busy CI runners.
        thread::sleep(Duration::from_millis(200));
        assert_eq!(kv_store.get_bytes("temp").await.unwrap(), None);
    }

    #[tokio::test]
    async fn ttl_not_expired_returns_value() {
        let (kv_store, _dir) = store();
        kv_store
            .put_bytes_with_ttl("temp", Bytes::from("val"), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(
            kv_store.get_bytes("temp").await.unwrap(),
            Some(Bytes::from("val"))
        );
    }

    #[tokio::test]
    async fn typed_roundtrip() {
        let (kv_store, _dir) = store();
        let cfg = Config {
            enabled: true,
            name: "test".into(),
        };
        kv_store.put("config", &cfg).await.unwrap();
        let out: Option<Config> = kv_store.get("config").await.unwrap();
        assert_eq!(out, Some(cfg));
    }

    #[tokio::test]
    async fn update_helper() {
        let (kv_store, _dir) = store();
        kv_store.put("counter", &0_i32).await.unwrap();
        let val = kv_store
            .read_modify_write("counter", 0_i32, |num| num + 5_i32)
            .await
            .unwrap();
        assert_eq!(val, 5_i32);
    }
}
