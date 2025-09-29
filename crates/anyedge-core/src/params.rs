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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn params(map: &[(&str, &str)]) -> PathParams {
        let inner = map
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        PathParams::new(inner)
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct StringParams {
        id: String,
    }

    #[test]
    fn get_returns_expected_value() {
        let params = params(&[("id", "7")]);
        assert_eq!(params.get("id"), Some("7"));
        assert_eq!(params.get("missing"), None);
    }

    #[test]
    fn deserialize_converts_to_target_type() {
        let params = params(&[("id", "42")]);
        let parsed: StringParams = params.deserialize().expect("params");
        assert_eq!(parsed, StringParams { id: "42".into() });
    }

    #[test]
    fn deserialize_propagates_errors() {
        #[allow(dead_code)]
        #[derive(Debug, Deserialize)]
        struct NumericParams {
            id: u32,
        }

        let params = params(&[("id", "not-a-number")]);
        let result: Result<NumericParams, _> = params.deserialize();
        assert!(result.is_err());
    }
}
