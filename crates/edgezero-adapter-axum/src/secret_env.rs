//! Naming rule that maps an Axum secret key to its environment variable.
//!
//! Compiled under both the `axum` and `cli` features so the runtime
//! `EnvSecretStore` and the CLI's typed-secret validation share one rule.

const PREFIX: &str = "EDGEZERO__SECRETS__";

/// Environment variable that holds Axum secret `key`.
///
/// The variable is `EDGEZERO__SECRETS__` followed by `key` in ASCII
/// uppercase. Axum has a single secret namespace, so the store name is not
/// part of the variable.
///
/// # Errors
/// Returns a message naming `key` when it is empty or contains anything
/// other than ASCII letters, digits, and `_`.
#[inline]
pub fn secret_env_var(key: &str) -> Result<String, String> {
    let valid = !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid {
        return Err(format!(
            "secret key {key:?} cannot name an Axum environment variable; use one or more ASCII letters, digits, or `_`"
        ));
    }
    Ok(format!("{PREFIX}{}", key.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_key_maps_to_prefixed_uppercase_variable() {
        assert_eq!(
            secret_env_var("demo_api_token").unwrap(),
            "EDGEZERO__SECRETS__DEMO_API_TOKEN"
        );
    }

    #[test]
    fn mixed_case_key_maps_to_uppercase_variable() {
        assert_eq!(
            secret_env_var("Api_Key").unwrap(),
            "EDGEZERO__SECRETS__API_KEY"
        );
    }

    #[test]
    fn digits_and_underscores_are_kept() {
        assert_eq!(
            secret_env_var("_key_2").unwrap(),
            "EDGEZERO__SECRETS___KEY_2"
        );
        assert_eq!(secret_env_var("123").unwrap(), "EDGEZERO__SECRETS__123");
    }

    #[test]
    fn empty_key_is_rejected() {
        let err = secret_env_var("").unwrap_err();
        assert!(err.contains("\"\""), "{err}");
    }

    #[test]
    fn keys_outside_ascii_alphanumeric_and_underscore_are_rejected() {
        for key in ["api-token", "api.token", "api token", "cl\u{e9}"] {
            let err = secret_env_var(key).unwrap_err();
            assert!(err.contains(key), "{key}: {err}");
            assert!(err.contains("ASCII letters, digits, or `_`"), "{err}");
        }
    }
}
