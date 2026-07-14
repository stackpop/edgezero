//! Happy-path coverage for `#[derive(AppConfig)]`. Compile-
//! fail coverage lives next to `tests/ui/*.rs` and runs via `trybuild`.

#[cfg(test)]
mod tests {
    use edgezero_core::app_config::{AppConfigMeta, AppConfigRoot, SecretKind};
    use validator::Validate as _;

    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct ConfigNoSecrets {
        _greeting: String,
    }

    // The `#[secret]`-annotated fields below are exercised only via the
    // `secret_fields()` method the derive emits — Rust still counts them
    // as "never read", so silence the dead-code lint at the struct level.
    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "fields exist only to feed `#[derive(AppConfig)]`; secret_fields() reads them via the derive, not via Rust field access"
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
        reason = "fields exist only to feed `#[derive(AppConfig)]`; secret_fields() reads them via the derive, not via Rust field access"
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
        reason = "fields exist only to feed `#[derive(AppConfig)]`; secret_fields() reads them via the derive, not via Rust field access"
    )]
    struct ConfigBothKinds {
        _greeting: String,
        #[secret]
        api_token: String,
        #[secret(store_ref)]
        vault: String,
    }

    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "fields exist only to feed `#[derive(AppConfig)]`; secret_fields() reads them via the derive, not via Rust field access"
    )]
    struct ConfigKeyInNamedStore {
        #[secret(store_ref = "vault")]
        api_token: String,
        #[secret(store_ref)]
        vault: String,
    }

    // Optional secret: `#[secret]` on `Option<String>` -> `optional: true`.
    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "fields exist only to feed `#[derive(AppConfig)]`; secret_fields() reads them via the derive, not via Rust field access"
    )]
    struct ConfigOptionalSecret {
        #[secret]
        api_token: Option<String>,
    }

    // Nested object + array recursion.
    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "fields exist only to feed `#[derive(AppConfig)]`; secret_fields() reads them via the derive, not via Rust field access"
    )]
    struct DataDome {
        #[secret]
        server_side_key: String,
    }

    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Integrations {
        #[app_config(nested)]
        #[validate(nested)]
        datadome: DataDome,
    }

    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "fields exist only to feed `#[derive(AppConfig)]`; secret_fields() reads them via the derive, not via Rust field access"
    )]
    struct Partner {
        #[secret]
        api_key: String,
        #[secret]
        maybe: Option<String>,
    }

    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Settings {
        #[app_config(nested)]
        #[validate(nested)]
        integrations: Integrations,
        #[app_config(nested)]
        #[validate(nested)]
        partners: Vec<Partner>,
    }

    /// Reflect each derived `SecretField` down to the tuple the
    /// assertions compare: `(dotted_path, kind, optional)`.
    fn reflect<C: AppConfigMeta>() -> Vec<(String, SecretKind, bool)> {
        C::secret_fields()
            .into_iter()
            .map(|field| (field.dotted_path(), field.kind, field.optional))
            .collect()
    }

    #[test]
    fn no_secret_annotation_yields_empty_secret_fields() {
        assert!(ConfigNoSecrets::secret_fields().is_empty());
    }

    #[test]
    fn plain_secret_attribute_yields_key_in_default() {
        assert_eq!(
            reflect::<ConfigKeyInDefault>(),
            vec![("api_token".to_owned(), SecretKind::KeyInDefault, false)]
        );
    }

    #[test]
    fn secret_store_ref_attribute_yields_store_ref() {
        assert_eq!(
            reflect::<ConfigStoreRef>(),
            vec![("vault".to_owned(), SecretKind::StoreRef, false)]
        );
    }

    #[test]
    fn both_secret_kinds_are_collected_in_source_order() {
        assert_eq!(
            reflect::<ConfigBothKinds>(),
            vec![
                ("api_token".to_owned(), SecretKind::KeyInDefault, false),
                ("vault".to_owned(), SecretKind::StoreRef, false),
            ]
        );
    }

    #[test]
    fn key_in_named_store_attribute_yields_correct_secret_fields() {
        assert_eq!(
            reflect::<ConfigKeyInNamedStore>(),
            vec![
                (
                    "api_token".to_owned(),
                    SecretKind::KeyInNamedStore {
                        store_ref_field: "vault",
                    },
                    false,
                ),
                ("vault".to_owned(), SecretKind::StoreRef, false),
            ]
        );
    }

    #[test]
    fn optional_string_secret_sets_optional_flag() {
        assert_eq!(
            reflect::<ConfigOptionalSecret>(),
            vec![("api_token".to_owned(), SecretKind::KeyInDefault, true)]
        );
    }

    #[test]
    fn nested_and_array_paths_are_emitted() {
        let mut paths = reflect::<Settings>();
        paths.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            paths,
            vec![
                (
                    "integrations.datadome.server_side_key".to_owned(),
                    SecretKind::KeyInDefault,
                    false,
                ),
                (
                    "partners[*].api_key".to_owned(),
                    SecretKind::KeyInDefault,
                    false
                ),
                (
                    "partners[*].maybe".to_owned(),
                    SecretKind::KeyInDefault,
                    true
                ),
            ],
        );
    }

    #[test]
    fn derive_emits_app_config_root_impl() {
        // The trait is a marker; we just need it to compile and the
        // blanket impl to be reachable via the trait object.
        fn assert_root<T: AppConfigRoot>() {}
        assert_root::<ConfigNoSecrets>();
        assert_root::<ConfigKeyInDefault>();
        assert_root::<ConfigStoreRef>();
        assert_root::<ConfigBothKinds>();
        assert_root::<ConfigKeyInNamedStore>();
    }

    #[test]
    fn trybuild_compile_fail_fixtures() {
        let cases = trybuild::TestCases::new();
        cases.compile_fail("tests/ui/secret_*.rs");
        cases.compile_fail("tests/ui/key_in_named_store_missing_sibling.rs");
        cases.compile_fail("tests/ui/key_in_named_store_sibling_not_store_ref.rs");
        cases.compile_fail("tests/ui/key_in_named_store_sibling_not_string.rs");
        // Spec 4.2 + 12.1: the serde-shape bans apply to EVERY
        // field, not just `#[secret]`-annotated ones. These three
        // fixtures pin the universal coverage the secret_*.rs
        // glob alone doesn't exercise.
        cases.compile_fail("tests/ui/non_secret_with_serde_flatten.rs");
        cases.compile_fail("tests/ui/non_secret_with_serde_skip_serializing.rs");
        cases.compile_fail("tests/ui/non_secret_with_serde_skip_serializing_if.rs");
        // `#[app_config(nested)]` recursion + `Option<String>` secret guards.
        // The `secret_*.rs` glob above already covers
        // `secret_on_option_non_string.rs` and `secret_store_ref_optional.rs`.
        cases.compile_fail("tests/ui/app_config_empty.rs");
        cases.compile_fail("tests/ui/app_config_nested_on_non_appconfig.rs");
        cases.compile_fail("tests/ui/app_config_nested_self_cycle.rs");
        cases.compile_fail("tests/ui/app_config_unknown_option.rs");
        cases.compile_fail("tests/ui/nested_field_serde_rename.rs");
        cases.compile_fail("tests/ui/nested_parent_rename_all.rs");
        cases.pass("tests/ui/secret_with_store_ref_named.rs");
        // A generic `#[app_config(nested)]` child compiles (method-scope bound check).
        cases.pass("tests/ui/app_config_generic_nested_child.rs");
    }
}
