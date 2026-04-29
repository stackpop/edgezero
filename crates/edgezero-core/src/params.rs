use std::collections::HashMap;

use serde::de::DeserializeOwned;

/// Normalised view of path parameters captured by the router.
#[derive(Clone, Debug, Default)]
pub struct PathParams {
    inner: HashMap<String, String>,
}

impl PathParams {
    /// # Errors
    /// Returns [`serde_json::Error`] if the path parameters cannot be deserialized into `T`.
    pub fn deserialize<T>(&self) -> Result<T, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        let value = serde_json::to_value(&self.inner)?;
        serde_json::from_value(value)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.inner.get(key).map(String::as_str)
    }

    #[must_use]
    pub fn new(inner: HashMap<String, String>) -> Self {
        Self { inner }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct StringParams {
        id: String,
    }

    fn params(map: &[(&str, &str)]) -> PathParams {
        let inner = map
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        PathParams::new(inner)
    }

    #[test]
    fn deserialize_converts_to_target_type() {
        let params = params(&[("id", "42")]);
        let parsed: StringParams = params.deserialize().expect("params");
        assert_eq!(parsed, StringParams { id: "42".into() });
    }

    #[test]
    fn deserialize_propagates_errors() {
        #[expect(dead_code, reason = "field exercised only via Deserialize")]
        #[derive(Debug, Deserialize)]
        struct NumericParams {
            id: u32,
        }

        let params = params(&[("id", "not-a-number")]);
        params
            .deserialize::<NumericParams>()
            .expect_err("`id` is not a number");
    }

    #[test]
    fn get_returns_expected_value() {
        let params = params(&[("id", "7")]);
        assert_eq!(params.get("id"), Some("7"));
        assert_eq!(params.get("missing"), None);
    }
}
