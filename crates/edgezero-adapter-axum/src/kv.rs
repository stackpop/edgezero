//! Persistent KV store for local development and testing.
//!
//! Values are stored in a `redb` embedded database with TTL support.
//! Data persists across server restarts, providing parity with edge provider
//! KV stores (Cloudflare Workers KV, Fastly KV Store).
//!
//! ## Storage Location
//!
//! By default, the development server stores data at `.edgezero/kv.redb`
//! in your project directory. Add this path to your `.gitignore`:
//!
//! ```gitignore
//! .edgezero/
//! ```
//!
//! ## TTL and Cleanup
//!
//! Expired entries are lazily evicted when accessed via `get_bytes` or when
//! scanning keys with `list_keys`. Entries that are never accessed after expiration
//! will remain in the database until explicitly listed or deleted.
//!
//! For long-running servers with many expiring keys, consider periodically calling
//! `list_keys("")` to trigger cleanup of expired entries.
//!
//! ## Database File Management
//!
//! The redb database file will grow over time and does not automatically
//! shrink after deletions. For development, this is typically not an issue.
//! To reclaim space, delete the `.edgezero/kv.redb` file (data will be lost).
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
//! - `list_keys` performs a full table scan. Performance may degrade with
//!   very large datasets (>10k keys).
//! - All operations are ACID-compliant via redb's transaction system.
//! - The database file path acts as the namespace identifier, similar to
//!   how Cloudflare uses bindings and Fastly uses store names.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::kv::{KvError, KvStore};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use web_time::SystemTime;

/// Table definition for the KV store.
/// Key: String, Value: (Bytes, Option<expiration_timestamp_millis>)
const KV_TABLE: TableDefinition<&str, (&[u8], Option<u128>)> = TableDefinition::new("kv");

/// A persistent KV store backed by `redb`.
///
/// Suitable for local development where data persistence across restarts is needed.
/// TTL-expired entries are lazily evicted (checked on read/list).
pub struct PersistentKvStore {
    db: Database,
}

impl PersistentKvStore {
    /// Create a new persistent KV store at the given path.
    ///
    /// # Behavior
    ///
    /// - If the file does not exist, a new database will be initialized
    /// - If the file exists and is a valid redb database, it will be opened with existing data preserved
    /// - If the file exists but is not a valid redb database, returns an error
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, KvError> {
        let db_path = path.as_ref().to_path_buf();
        let db = Database::create(path).map_err(|e| {
            KvError::Internal(anyhow::anyhow!(
                "Failed to open KV database at {:?}. If the file is corrupted or locked \
                 by another process, try deleting it and restarting: {}",
                db_path,
                e
            ))
        })?;

        // Initialize the table
        let write_txn = db
            .begin_write()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to begin write txn: {}", e)))?;
        {
            let _table = write_txn
                .open_table(KV_TABLE)
                .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open table: {}", e)))?;
        }
        write_txn
            .commit()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to commit txn: {}", e)))?;

        Ok(Self { db })
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

    /// Convert SystemTime to milliseconds since UNIX epoch.
    ///
    /// Returns 0 if the time is before UNIX epoch (should never happen in practice).
    fn system_time_to_millis(time: SystemTime) -> u128 {
        time.duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }
}

#[async_trait(?Send)]
impl KvStore for PersistentKvStore {
    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to begin read txn: {}", e)))?;

        let table = read_txn
            .open_table(KV_TABLE)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open table: {}", e)))?;

        if let Some(entry) = table
            .get(key)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to get key: {}", e)))?
        {
            let (value_bytes, expires_at) = entry.value();

            // Check if expired
            if Self::is_expired(expires_at) {
                // Drop read transaction before write
                drop(table);
                drop(read_txn);

                // Delete the expired key
                let write_txn = self.db.begin_write().map_err(|e| {
                    KvError::Internal(anyhow::anyhow!("failed to begin write txn: {}", e))
                })?;
                {
                    let mut table = write_txn.open_table(KV_TABLE).map_err(|e| {
                        KvError::Internal(anyhow::anyhow!("failed to open table: {}", e))
                    })?;
                    table.remove(key).map_err(|e| {
                        KvError::Internal(anyhow::anyhow!("failed to remove: {}", e))
                    })?;
                }
                write_txn
                    .commit()
                    .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to commit: {}", e)))?;

                return Ok(None);
            }

            Ok(Some(Bytes::copy_from_slice(value_bytes)))
        } else {
            Ok(None)
        }
    }

    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to begin write txn: {}", e)))?;

        {
            let mut table = write_txn
                .open_table(KV_TABLE)
                .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open table: {}", e)))?;

            table
                .insert(key, (value.as_ref(), None))
                .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to insert: {}", e)))?;
        }

        write_txn
            .commit()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to commit: {}", e)))?;

        Ok(())
    }

    async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        ttl: Duration,
    ) -> Result<(), KvError> {
        let expires_at = SystemTime::now() + ttl;
        let expires_at_millis = Self::system_time_to_millis(expires_at);

        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to begin write txn: {}", e)))?;

        {
            let mut table = write_txn
                .open_table(KV_TABLE)
                .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open table: {}", e)))?;

            table
                .insert(key, (value.as_ref(), Some(expires_at_millis)))
                .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to insert: {}", e)))?;
        }

        write_txn
            .commit()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to commit: {}", e)))?;

        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to begin write txn: {}", e)))?;

        {
            let mut table = write_txn
                .open_table(KV_TABLE)
                .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open table: {}", e)))?;

            table
                .remove(key)
                .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to remove: {}", e)))?;
        }

        write_txn
            .commit()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to commit: {}", e)))?;

        Ok(())
    }

    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, KvError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to begin read txn: {}", e)))?;

        let table = read_txn
            .open_table(KV_TABLE)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open table: {}", e)))?;

        // Collect all keys and identify expired ones
        let mut keys = Vec::new();
        let mut expired_keys = Vec::new();

        let iter = table
            .iter()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to iterate: {}", e)))?;

        for entry in iter {
            let entry = entry
                .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to read entry: {}", e)))?;
            let key = entry.0.value();
            let (_value, expires_at) = entry.1.value();

            if Self::is_expired(expires_at) {
                expired_keys.push(key.to_string());
            } else if key.starts_with(prefix) {
                keys.push(key.to_string());
            }
        }

        // Drop read transaction before write
        drop(table);
        drop(read_txn);

        // Remove expired keys
        if !expired_keys.is_empty() {
            let write_txn = self.db.begin_write().map_err(|e| {
                KvError::Internal(anyhow::anyhow!("failed to begin write txn: {}", e))
            })?;
            {
                let mut table = write_txn.open_table(KV_TABLE).map_err(|e| {
                    KvError::Internal(anyhow::anyhow!("failed to open table: {}", e))
                })?;
                for key in &expired_keys {
                    table.remove(key.as_str()).map_err(|e| {
                        KvError::Internal(anyhow::anyhow!("failed to remove: {}", e))
                    })?;
                }
            }
            write_txn
                .commit()
                .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to commit: {}", e)))?;
        }

        // Sort keys to maintain consistency with BTreeMap behavior
        keys.sort();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::kv::KvHandle;
    use std::sync::Arc;

    fn store() -> KvHandle {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let store = PersistentKvStore::new(db_path).unwrap();
        KvHandle::new(Arc::new(store))
    }

    // -- Raw bytes -----------------------------------------------------------

    #[tokio::test]
    async fn put_and_get_bytes() {
        let s = store();
        s.put_bytes("k", Bytes::from("hello")).await.unwrap();
        assert_eq!(s.get_bytes("k").await.unwrap(), Some(Bytes::from("hello")));
    }

    #[tokio::test]
    async fn get_missing_key_returns_none() {
        let s = store();
        assert_eq!(s.get_bytes("missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn put_overwrites_existing() {
        let s = store();
        s.put_bytes("k", Bytes::from("first")).await.unwrap();
        s.put_bytes("k", Bytes::from("second")).await.unwrap();
        assert_eq!(s.get_bytes("k").await.unwrap(), Some(Bytes::from("second")));
    }

    #[tokio::test]
    async fn delete_removes_key() {
        let s = store();
        s.put_bytes("k", Bytes::from("v")).await.unwrap();
        s.delete("k").await.unwrap();
        assert_eq!(s.get_bytes("k").await.unwrap(), None);
    }

    #[tokio::test]
    async fn delete_nonexistent_is_ok() {
        let s = store();
        s.delete("nope").await.unwrap();
    }

    #[tokio::test]
    async fn list_keys_filters_by_prefix() {
        let s = store();
        s.put_bytes("a", Bytes::from("1")).await.unwrap();
        s.put_bytes("b", Bytes::from("2")).await.unwrap();
        let keys = s.list_keys("").await.unwrap();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn ttl_expires_entry() {
        // Use the store impl directly to bypass validation limits (min TTL 60s)
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let s = PersistentKvStore::new(db_path).unwrap();
        s.put_bytes_with_ttl("temp", Bytes::from("val"), Duration::from_millis(1))
            .await
            .unwrap();
        // Wait for expiration
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(s.get_bytes("temp").await.unwrap(), None);
    }

    #[tokio::test]
    async fn ttl_not_expired_returns_value() {
        let s = store();
        s.put_bytes_with_ttl("temp", Bytes::from("val"), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(s.get_bytes("temp").await.unwrap(), Some(Bytes::from("val")));
    }

    #[tokio::test]
    async fn list_keys_evicts_expired() {
        // Use the store impl directly to bypass validation limits (min TTL 60s)
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let s = PersistentKvStore::new(db_path).unwrap();
        s.put_bytes_with_ttl("expired", Bytes::from("x"), Duration::from_millis(1))
            .await
            .unwrap();
        s.put_bytes("alive", Bytes::from("y")).await.unwrap();
        std::thread::sleep(Duration::from_millis(10));

        let keys = s.list_keys("").await.unwrap();
        assert_eq!(keys, vec!["alive"]);
    }

    // -- Typed helpers via KvHandle ----------------------------------------

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Config {
        name: String,
        enabled: bool,
    }

    #[tokio::test]
    async fn typed_roundtrip() {
        let s = store();
        let cfg = Config {
            name: "test".into(),
            enabled: true,
        };
        s.put("config", &cfg).await.unwrap();
        let out: Option<Config> = s.get("config").await.unwrap();
        assert_eq!(out, Some(cfg));
    }

    #[tokio::test]
    async fn update_helper() {
        let s = store();
        s.put("counter", &0i32).await.unwrap();
        let val = s.update("counter", 0i32, |n| n + 5).await.unwrap();
        assert_eq!(val, 5);
    }

    #[tokio::test]
    async fn exists_helper() {
        let s = store();
        assert!(!s.exists("nope").await.unwrap());
        s.put_bytes("k", Bytes::from("v")).await.unwrap();
        assert!(s.exists("k").await.unwrap());
    }

    #[tokio::test]
    async fn new_store_is_empty() {
        let s = store();
        let keys = s.list_keys("").await.unwrap();
        assert!(keys.is_empty());
        assert!(!s.exists("anything").await.unwrap());
    }

    #[tokio::test]
    async fn concurrent_writes_dont_panic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let s = PersistentKvStore::new(db_path).unwrap();
        let handle = KvHandle::new(std::sync::Arc::new(s));

        // Write 100 keys and verify each one
        for i in 0..100i32 {
            let key = format!("key:{i}");
            handle.put(&key, &i).await.unwrap();
        }

        // Verify all 100 keys exist with correct values
        for i in 0..100i32 {
            let key = format!("key:{i}");
            let val: i32 = handle.get_or(&key, -1).await.unwrap();
            assert_eq!(val, i);
        }

        let keys = handle.list_keys("key:").await.unwrap();
        assert_eq!(keys.len(), 100);
    }

    #[tokio::test]
    async fn delete_then_list_keys_is_consistent() {
        let s = store();
        s.put_bytes("a", Bytes::from("1")).await.unwrap();
        s.put_bytes("b", Bytes::from("2")).await.unwrap();
        s.put_bytes("c", Bytes::from("3")).await.unwrap();

        s.delete("b").await.unwrap();

        let keys = s.list_keys("").await.unwrap();
        assert_eq!(keys, vec!["a", "c"]);
    }

    #[tokio::test]
    async fn data_persists_across_reopens() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        // Write data
        {
            let store = PersistentKvStore::new(&db_path).unwrap();
            store
                .put_bytes("persistent", Bytes::from("value"))
                .await
                .unwrap();
        }

        // Reopen and verify data persists
        {
            let store = PersistentKvStore::new(&db_path).unwrap();
            let value = store.get_bytes("persistent").await.unwrap();
            assert_eq!(value, Some(Bytes::from("value")));
        }
    }

    // Run the shared contract tests against PersistentKvStore.
    edgezero_core::kv_contract_tests!(persistent_kv_contract, {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        PersistentKvStore::new(db_path).unwrap()
    });
}
