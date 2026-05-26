//! Happy-path coverage for `#[derive(AppConfig)]` (Task 3.2). Compile-
//! fail coverage lives next to `tests/ui/*.rs` and runs via `trybuild`.

#[cfg(test)]
mod tests {
    use edgezero_core::app_config::{AppConfigMeta as _, SecretField, SecretKind};

    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct ConfigNoSecrets {
        _greeting: String,
    }

    // The `#[secret]`-annotated fields below are exercised only via the
    // `SECRET_FIELDS` associated constant the derive emits — Rust still
    // counts them as "never read", so silence the dead-code lint at the
    // struct level.
    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "fields exist only to feed `#[derive(AppConfig)]`; the SECRET_FIELDS array reads them via the derive, not via Rust field access"
    )]
    struct ConfigKeyInDefault {
        _greeting: String,
        #[secret]
        api_token: String,
    }

    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "fields exist only to feed `#[derive(AppConfig)]`; the SECRET_FIELDS array reads them via the derive, not via Rust field access"
    )]
    struct ConfigStoreRef {
        _greeting: String,
        #[secret(store_ref)]
        vault: String,
    }

    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "fields exist only to feed `#[derive(AppConfig)]`; the SECRET_FIELDS array reads them via the derive, not via Rust field access"
    )]
    struct ConfigBothKinds {
        _greeting: String,
        #[secret]
        api_token: String,
        #[secret(store_ref)]
        vault: String,
    }

    #[test]
    fn no_secret_annotation_yields_empty_secret_fields() {
        assert!(ConfigNoSecrets::SECRET_FIELDS.is_empty());
    }

    #[test]
    fn plain_secret_attribute_yields_key_in_default() {
        assert_eq!(
            ConfigKeyInDefault::SECRET_FIELDS,
            &[SecretField {
                name: "api_token",
                kind: SecretKind::KeyInDefault,
            }]
        );
    }

    #[test]
    fn secret_store_ref_attribute_yields_store_ref() {
        assert_eq!(
            ConfigStoreRef::SECRET_FIELDS,
            &[SecretField {
                name: "vault",
                kind: SecretKind::StoreRef,
            }]
        );
    }

    #[test]
    fn both_secret_kinds_are_collected_in_source_order() {
        assert_eq!(
            ConfigBothKinds::SECRET_FIELDS,
            &[
                SecretField {
                    name: "api_token",
                    kind: SecretKind::KeyInDefault,
                },
                SecretField {
                    name: "vault",
                    kind: SecretKind::StoreRef,
                },
            ]
        );
    }

    #[test]
    fn trybuild_compile_fail_fixtures() {
        let cases = trybuild::TestCases::new();
        cases.compile_fail("tests/ui/secret_*.rs");
    }
}
