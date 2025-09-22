use std::collections::HashMap;

use serde::de::DeserializeOwned;

/// Normalised view of path parameters captured by the router.
#[derive(Clone, Debug, Default)]
pub struct PathParams {
    inner: HashMap<String, String>,
}

impl PathParams {
    pub fn new(inner: HashMap<String, String>) -> Self {
        Self { inner }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.inner.get(key).map(|s| s.as_str())
    }

    pub fn deserialize<T>(&self) -> Result<T, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        let value = serde_json::to_value(&self.inner)?;
        serde_json::from_value(value)
    }
}
