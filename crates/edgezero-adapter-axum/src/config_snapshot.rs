//! Flat-string snapshots: no intermediate JSON tree or per-read payload copies.

use std::fmt;
use std::io::Read;
use std::mem::size_of;

use edgezero_core::ConfigValue;
use edgezero_core::config_store::ConfigStoreError;
use serde::Deserializer;
use serde::de::{self, DeserializeSeed, Error as _, MapAccess, Visitor};

use crate::config_store_limits::ConfigStoreLimits;

pub(crate) struct Snapshot {
    entries: Vec<(String, ConfigValue)>,
    resident_bytes: usize,
}

struct SnapshotSeed<'snapshot> {
    existing: usize,
    failure: &'snapshot mut Option<ConfigStoreError>,
    limits: ConfigStoreLimits,
    snapshot: &'snapshot mut Snapshot,
}

struct StringSeed<'failure> {
    failure: &'failure mut Option<ConfigStoreError>,
    max_bytes: usize,
}

impl Snapshot {
    pub(crate) fn empty(
        limits: ConfigStoreLimits,
        existing: usize,
    ) -> Result<Self, ConfigStoreError> {
        let index_bytes = limits
            .max_entries()
            .checked_mul(size_of::<(String, ConfigValue)>())
            .ok_or(ConfigStoreError::ValueTooLarge)?;
        check_charge(limits, existing, index_bytes)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(limits.max_entries())
            .map_err(|_allocation| {
                ConfigStoreError::internal(anyhow::anyhow!("config snapshot allocation failed"))
            })?;
        Ok(Self {
            resident_bytes: entries
                .capacity()
                .checked_mul(size_of::<(String, ConfigValue)>())
                .ok_or(ConfigStoreError::ValueTooLarge)?,
            entries,
        })
    }

    fn finish(&mut self) -> Result<(), ConfigStoreError> {
        self.entries
            .sort_unstable_by(|(left, _value), (right, _other)| left.cmp(right));
        if self.entries.windows(2).any(|pair| {
            pair.first()
                .zip(pair.get(1))
                .is_some_and(|(left, right)| left.0 == right.0)
        }) {
            return Err(ConfigStoreError::unavailable(
                "duplicate config snapshot key",
            ));
        }
        Ok(())
    }

    pub(crate) fn from_entries<E>(
        entries: E,
        limits: ConfigStoreLimits,
    ) -> Result<Self, ConfigStoreError>
    where
        E: IntoIterator<Item = (String, String)>,
    {
        let mut snapshot = Self::empty(limits, 0)?;
        for (key, value) in entries {
            snapshot.insert(&key, &value, limits, 0)?;
        }
        snapshot.finish()?;
        Ok(snapshot)
    }

    pub(crate) fn get(&self, key: &str) -> Option<&ConfigValue> {
        self.entries
            .binary_search_by(|(stored, _value)| stored.as_str().cmp(key))
            .ok()
            .and_then(|index| self.entries.get(index))
            .map(|(_key, value)| value)
    }

    fn insert(
        &mut self,
        key: &str,
        value: &str,
        limits: ConfigStoreLimits,
        existing: usize,
    ) -> Result<(), ConfigStoreError> {
        if self.entries.len() >= limits.max_entries()
            || key.len() > limits.max_key_bytes()
            || value.len() > limits.max_value_bytes()
        {
            return Err(ConfigStoreError::ValueTooLarge);
        }
        // The Vec index was reserved already. Strings have exact requested size;
        // Arc<str> has two reference counts in addition to the UTF-8 payload.
        let charge = self
            .resident_bytes
            .checked_add(key.len())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .and_then(|bytes| {
                bytes.checked_add(size_of::<usize>().saturating_mul(3).saturating_sub(1))
            })
            .ok_or(ConfigStoreError::ValueTooLarge)?;
        check_charge(limits, existing, charge)?;
        self.entries
            .push((key.to_owned(), ConfigValue::from(value)));
        self.resident_bytes = charge;
        Ok(())
    }

    pub(crate) fn load(
        reader: &mut impl Read,
        limits: ConfigStoreLimits,
        existing: usize,
    ) -> Result<Self, ConfigStoreError> {
        let mut snapshot = Self::empty(limits, existing)?;
        let raw = read_capped(reader, limits.max_file_bytes())?;
        // Reject non-object roots before Serde can format their contents into
        // an expanded type-error diagnostic outside the decoder scratch budget.
        if raw
            .iter()
            .copied()
            .find(|byte| !matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
            != Some(b'{')
        {
            return Err(ConfigStoreError::unavailable(
                "config snapshot requires an object",
            ));
        }
        let mut decoder = serde_json::Deserializer::from_slice(&raw);
        let mut failure = None;
        let result = SnapshotSeed {
            snapshot: &mut snapshot,
            limits,
            existing,
            failure: &mut failure,
        }
        .deserialize(&mut decoder)
        .and_then(|()| decoder.end());
        if result.is_err() {
            return Err(
                failure.unwrap_or_else(|| ConfigStoreError::unavailable("invalid config snapshot"))
            );
        }
        snapshot.finish()?;
        Ok(snapshot)
    }

    pub(crate) const fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }
}

impl<'de> DeserializeSeed<'de> for SnapshotSeed<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

#[expect(
    clippy::missing_trait_methods,
    reason = "all non-map visitor methods intentionally retain Serde's type rejection"
)]
impl<'de> Visitor<'de> for SnapshotSeed<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a flat string map")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key_seed(StringSeed {
            max_bytes: self.limits.max_key_bytes(),
            failure: self.failure,
        })? {
            if self.snapshot.entries.len() >= self.limits.max_entries() {
                *self.failure = Some(ConfigStoreError::ValueTooLarge);
                return Err(A::Error::custom("snapshot limit"));
            }
            let value = map.next_value_seed(StringSeed {
                max_bytes: self.limits.max_value_bytes(),
                failure: self.failure,
            })?;
            if let Err(error) = self
                .snapshot
                .insert(&key, &value, self.limits, self.existing)
            {
                *self.failure = Some(error);
                return Err(A::Error::custom("snapshot limit"));
            }
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for StringSeed<'_> {
    type Value = String;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        deserializer.deserialize_str(self)
    }
}

#[expect(
    clippy::missing_trait_methods,
    reason = "Serde's borrowed string method forwards to the bounded visit_str; non-strings are rejected by default"
)]
impl Visitor<'_> for StringSeed<'_> {
    type Value = String;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded UTF-8 string")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
        if v.len() > self.max_bytes {
            *self.failure = Some(ConfigStoreError::ValueTooLarge);
            return Err(E::custom("snapshot string limit"));
        }
        Ok(v.to_owned())
    }
}

fn check_charge(
    limits: ConfigStoreLimits,
    existing: usize,
    additional: usize,
) -> Result<(), ConfigStoreError> {
    let resident = existing
        .checked_add(additional)
        .ok_or(ConfigStoreError::ValueTooLarge)?;
    if resident > limits.max_resident_bytes()
        || resident.saturating_add(limits.staging_bytes()) > limits.max_startup_bytes()
    {
        return Err(ConfigStoreError::ValueTooLarge);
    }
    Ok(())
}

fn read_capped(reader: &mut impl Read, max_bytes: usize) -> Result<Vec<u8>, ConfigStoreError> {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let remaining = max_bytes.saturating_sub(raw.len());
        let read_len = chunk.len().min(remaining.saturating_add(1));
        let read = reader
            .read(
                chunk
                    .get_mut(..read_len)
                    .ok_or(ConfigStoreError::ValueTooLarge)?,
            )
            .map_err(|_io| ConfigStoreError::unavailable("config snapshot read failed"))?;
        if read == 0 {
            return Ok(raw);
        }
        if read > remaining {
            return Err(ConfigStoreError::ValueTooLarge);
        }
        raw.try_reserve_exact(read).map_err(|_allocation| {
            ConfigStoreError::internal(anyhow::anyhow!("config snapshot allocation failed"))
        })?;
        raw.extend_from_slice(chunk.get(..read).ok_or(ConfigStoreError::ValueTooLarge)?);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn limits(file: usize, entries: usize, key: usize, value: usize) -> ConfigStoreLimits {
        ConfigStoreLimits::new(file, entries, key, value, 8192, 0x8000).expect("limits")
    }

    #[test]
    fn file_growth_is_capped_by_reads_not_metadata() {
        let mut reader = Cursor::new(vec![b' '; 9]);
        assert!(matches!(
            read_capped(&mut reader, 8),
            Err(ConfigStoreError::ValueTooLarge)
        ));
        assert_eq!(reader.position(), 9);
        assert_eq!(
            read_capped(&mut Cursor::new(vec![b' '; 8]), 8).expect("exact"),
            vec![b' '; 8]
        );
    }

    #[test]
    fn malformed_root_strings_are_rejected_before_diagnostic_expansion() {
        let raw = serde_json::to_vec(&serde_json::json!("\u{0001}".repeat(8192))).expect("fixture");
        let policy = ConfigStoreLimits::new(
            raw.len(),
            1,
            1,
            1,
            4096,
            raw.len().saturating_mul(5).saturating_add(4096),
        )
        .expect("policy");
        let error = Snapshot::load(&mut raw.as_slice(), policy, 0)
            .err()
            .expect("non-object");
        assert_eq!(
            error.to_string(),
            ConfigStoreError::unavailable("config snapshot requires an object").to_string()
        );
    }

    #[test]
    fn escaped_values_and_entry_limits_are_enforced() {
        let raw = serde_json::to_vec(&serde_json::json!({"a": "\n\n\n"})).expect("fixture");
        let _snapshot = Snapshot::load(&mut raw.as_slice(), limits(128, 1, 1, 3), 0)
            .expect("exact decoded cap");
        assert!(matches!(
            Snapshot::load(&mut raw.as_slice(), limits(128, 1, 1, 2), 0),
            Err(ConfigStoreError::ValueTooLarge)
        ));
        let many = serde_json::to_vec(&serde_json::json!({"a": "a", "b": "b"})).expect("fixture");
        assert!(matches!(
            Snapshot::load(&mut many.as_slice(), limits(128, 1, 1, 3), 0),
            Err(ConfigStoreError::ValueTooLarge)
        ));
    }

    #[test]
    fn all_store_residency_is_reserved_before_loading() {
        let limits = limits(128, 1, 1, 3);
        let raw = serde_json::to_vec(&serde_json::json!({"a": "abc"})).expect("fixture");
        let first = Snapshot::load(&mut raw.as_slice(), limits, 0).expect("snapshot");
        assert!(matches!(
            Snapshot::load(
                &mut raw.as_slice(),
                limits,
                limits.max_resident_bytes() - first.resident_bytes() + 1
            ),
            Err(ConfigStoreError::ValueTooLarge)
        ));
        let _recovered = Snapshot::load(&mut raw.as_slice(), limits, 0)
            .expect("failed load has no retained charge");
    }
}
