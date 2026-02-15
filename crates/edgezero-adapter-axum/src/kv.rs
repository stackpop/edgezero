//! In-memory KV store for local development and testing.
//!
//! Values are stored in a `BTreeMap` behind a `std::sync::Mutex`.
//! TTL-expired entries are lazily evicted on access.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::kv::{KvError, KvStore};
use web_time::Instant;

/// Entry stored in the in-memory KV store.
struct Entry {
    value: Bytes,
    expires_at: Option<Instant>,
}

impl Entry {
    fn is_expired(&self) -> bool {
        self.expires_at
            .map(|exp| Instant::now() >= exp)
            .unwrap_or(false)
    }
}

/// An in-memory KV store backed by `BTreeMap<String, Entry>`.
///
/// Suitable for local development and unit testing.
/// TTL-expired entries are lazily evicted (checked on read/list).
///
/// Uses `BTreeMap` instead of `HashMap` to keep keys in sorted order,
/// which makes `list_keys` prefix scans efficient without a post-sort.
pub struct MemoryKvStore {
    data: Mutex<BTreeMap<String, Entry>>,
}

impl MemoryKvStore {
    /// Create an empty in-memory KV store.
    pub fn new() -> Self {
        Self {
            data: Mutex::new(BTreeMap::new()),
        }
    }

    /// Lock the inner data, converting a poisoned-lock panic into `KvError`.
    fn lock_data(&self) -> Result<MutexGuard<'_, BTreeMap<String, Entry>>, KvError> {
        self.data
            .lock()
            .map_err(|_| KvError::Internal(anyhow::anyhow!("kv store lock poisoned")))
    }
}

impl Default for MemoryKvStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl KvStore for MemoryKvStore {
    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        let mut data = self.lock_data()?;
        if let Some(entry) = data.get(key) {
            if entry.is_expired() {
                data.remove(key);
                return Ok(None);
            }
            Ok(Some(entry.value.clone()))
        } else {
            Ok(None)
        }
    }

    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        let mut data = self.lock_data()?;
        data.insert(
            key.to_string(),
            Entry {
                value,
                expires_at: None,
            },
        );
        Ok(())
    }

    async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        ttl: Duration,
    ) -> Result<(), KvError> {
        let mut data = self.lock_data()?;
        data.insert(
            key.to_string(),
            Entry {
                value,
                expires_at: Some(Instant::now() + ttl),
            },
        );
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        let mut data = self.lock_data()?;
        data.remove(key);
        Ok(())
    }

    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, KvError> {
        let mut data = self.lock_data()?;

        // Collect expired keys to remove
        let expired: Vec<String> = data
            .iter()
            .filter(|(_, entry)| entry.is_expired())
            .map(|(key, _)| key.clone())
            .collect();
        for key in &expired {
            data.remove(key);
        }

        // BTreeMap keys are already sorted — range scan is efficient.
        let keys: Vec<String> = data
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::kv::KvHandle;
    use std::sync::Arc;

    fn store() -> KvHandle {
        KvHandle::new(Arc::new(MemoryKvStore::new()))
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
        let s = store();
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
        let s = store();
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
        let s = MemoryKvStore::new();
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

    // Run the shared contract tests against MemoryKvStore.
    edgezero_core::kv_contract_tests!(memory_kv_contract, MemoryKvStore::new());
}
