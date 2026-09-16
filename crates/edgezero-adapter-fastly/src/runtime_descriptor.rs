use crate::RuntimeDescriptorError;
#[cfg(any(feature = "fastly", test))]
use crate::RuntimeEnvConfigError;
use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
use edgezero_core::app::StoresMetadata;
use edgezero_core::env_config::EnvConfig;
use serde::de::{Error as _, IgnoredAny, MapAccess, Visitor};
use serde::ser::SerializeStruct as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
#[cfg(any(feature = "cli", test))]
use std::collections::BTreeSet;
#[cfg(any(feature = "fastly", test))]
use std::error::Error;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr as _;

pub(crate) const RUNTIME_DESCRIPTOR_FORMAT: u8 = 1;

const FIXED_RUNTIME_KEYS: [(&str, RuntimeValueKind); 6] = [
    ("EDGEZERO__ADAPTER__HOST", RuntimeValueKind::IpAddress),
    ("EDGEZERO__ADAPTER__PORT", RuntimeValueKind::Port),
    ("EDGEZERO__LOGGING__LEVEL", RuntimeValueKind::LogLevel),
    (
        "EDGEZERO__LOGGING__ENDPOINT",
        RuntimeValueKind::NonblankPrintable,
    ),
    (
        "EDGEZERO__LOGGING__USE_FASTLY_LOGGER",
        RuntimeValueKind::Boolean,
    ),
    ("EDGEZERO__LOGGING__ECHO_STDOUT", RuntimeValueKind::Boolean),
];

#[derive(Debug)]
#[cfg(any(feature = "fastly", test))]
pub(crate) enum RuntimeEnvLookupError<E> {
    Lookup(E),
    SelectorStoreAbsent,
}

#[derive(Debug)]
#[cfg(any(feature = "fastly", test))]
pub(crate) struct RuntimeEnvConfigResolution {
    env: EnvConfig,
    fallback: Option<RuntimeEnvFallbackDiagnostic>,
}

#[cfg(any(feature = "fastly", test))]
impl RuntimeEnvConfigResolution {
    #[cfg(feature = "fastly")]
    pub(crate) fn into_parts(self) -> (EnvConfig, Option<RuntimeEnvFallbackDiagnostic>) {
        (self.env, self.fallback)
    }
}

#[derive(Clone, Copy, Debug)]
#[cfg(any(feature = "fastly", test))]
enum RuntimeEnvFallbackReason {
    DescriptorAbsent,
    SelectorStoreAbsent,
}

#[derive(Debug)]
#[cfg(any(feature = "fastly", test))]
pub(crate) struct RuntimeEnvFallbackDiagnostic {
    descriptor_key: String,
    reason: RuntimeEnvFallbackReason,
}

#[cfg(any(feature = "fastly", test))]
impl fmt::Display for RuntimeEnvFallbackDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let missing = match self.reason {
            RuntimeEnvFallbackReason::DescriptorAbsent => "runtime descriptor is absent",
            RuntimeEnvFallbackReason::SelectorStoreAbsent => {
                "selector Config Store `edgezero_runtime_env` is absent"
            }
        };
        write!(
            f,
            "{missing} for `{}`; using baked-in defaults because the app declares no stores",
            self.descriptor_key
        )
    }
}

#[derive(Debug)]
pub(crate) struct RuntimeDescriptor {
    entries: BTreeMap<String, String>,
}

struct RuntimeDescriptorFormat(u64);

struct RuntimeDescriptorWire {
    entries: BTreeMap<String, String>,
    format: u8,
}

impl RuntimeDescriptor {
    pub(crate) fn canonical_json(&self) -> Result<String, RuntimeDescriptorError> {
        let json = serde_json::to_string(self)
            .map_err(|_serialize_error| RuntimeDescriptorError::InvalidJson)?;
        if json.len() > FASTLY_CONFIG_ENTRY_LIMIT {
            return Err(RuntimeDescriptorError::EntryTooLarge);
        }
        Ok(json)
    }

    #[cfg(any(feature = "cli", test))]
    pub(crate) fn from_entries(
        entries: BTreeMap<String, String>,
    ) -> Result<Self, RuntimeDescriptorError> {
        let descriptor = Self { entries };
        descriptor.canonical_json()?;
        Ok(descriptor)
    }

    #[cfg(feature = "cli")]
    pub(crate) fn from_environment(
        environment: &EnvConfig,
        config_ids: &[String],
        kv_ids: &[String],
        secret_ids: &[String],
    ) -> Result<Self, RuntimeDescriptorError> {
        let mut entries = BTreeMap::new();
        for (key, segments) in [
            ("EDGEZERO__ADAPTER__HOST", &["adapter", "host"][..]),
            ("EDGEZERO__ADAPTER__PORT", &["adapter", "port"][..]),
            ("EDGEZERO__LOGGING__LEVEL", &["logging", "level"][..]),
            ("EDGEZERO__LOGGING__ENDPOINT", &["logging", "endpoint"][..]),
            (
                "EDGEZERO__LOGGING__USE_FASTLY_LOGGER",
                &["logging", "use_fastly_logger"][..],
            ),
            (
                "EDGEZERO__LOGGING__ECHO_STDOUT",
                &["logging", "echo_stdout"][..],
            ),
        ] {
            if let Some(value) = environment.get(segments) {
                entries.insert(key.to_owned(), value.to_owned());
            }
        }
        for (kind, ids) in [
            ("CONFIG", config_ids),
            ("KV", kv_ids),
            ("SECRETS", secret_ids),
        ] {
            for id in ids {
                let canonical_id = id.to_ascii_uppercase();
                entries.insert(
                    format!("EDGEZERO__STORES__{kind}__{canonical_id}__NAME"),
                    environment.store_name(&kind.to_ascii_lowercase(), id),
                );
                if kind == "CONFIG" {
                    entries.insert(
                        format!("EDGEZERO__STORES__CONFIG__{canonical_id}__KEY"),
                        environment.store_key("config", id),
                    );
                }
            }
        }
        Self::from_entries(entries)
    }

    #[cfg(any(feature = "cli", test))]
    pub(crate) fn managed_store_aliases(&self) -> Result<BTreeSet<String>, RuntimeDescriptorError> {
        let mut aliases = BTreeSet::new();
        for (key, value) in &self.entries {
            if is_managed_store_name_key(key) {
                if !is_nonblank_printable(value) {
                    return Err(RuntimeDescriptorError::InvalidValue { key: key.clone() });
                }
                aliases.insert(value.clone());
            }
        }
        Ok(aliases)
    }

    pub(crate) fn parse(raw: &str) -> Result<Self, RuntimeDescriptorError> {
        if raw.len() > FASTLY_CONFIG_ENTRY_LIMIT {
            return Err(RuntimeDescriptorError::EntryTooLarge);
        }
        let RuntimeDescriptorFormat(format) = serde_json::from_str(raw)
            .map_err(|_probe_error| RuntimeDescriptorError::InvalidJson)?;
        if format != u64::from(RUNTIME_DESCRIPTOR_FORMAT) {
            return Err(RuntimeDescriptorError::UnsupportedFormat { format });
        }
        let wire: RuntimeDescriptorWire = serde_json::from_str(raw)
            .map_err(|_parse_error| RuntimeDescriptorError::InvalidJson)?;
        if wire.format != RUNTIME_DESCRIPTOR_FORMAT {
            return Err(RuntimeDescriptorError::InvalidJson);
        }
        let descriptor = Self {
            entries: wire.entries,
        };
        descriptor.canonical_json()?;
        Ok(descriptor)
    }

    pub(crate) fn validated_env(
        &self,
        stores: StoresMetadata,
    ) -> Result<EnvConfig, RuntimeDescriptorError> {
        let allowed_keys = runtime_value_kinds(stores);
        validated_env_from_entries(&self.entries, &allowed_keys)
    }
}

impl Serialize for RuntimeDescriptor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("RuntimeDescriptor", 2)?;
        state.serialize_field("format", &RUNTIME_DESCRIPTOR_FORMAT)?;
        state.serialize_field("entries", &self.entries)?;
        state.end()
    }
}

#[expect(
    clippy::missing_trait_methods,
    reason = "serde's default deserialize_in_place implementation is sufficient"
)]
impl<'de> Deserialize<'de> for RuntimeDescriptorFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(RuntimeDescriptorFormatVisitor)
    }
}

struct RuntimeDescriptorFormatVisitor;

#[expect(
    clippy::missing_trait_methods,
    reason = "only JSON maps are valid runtime descriptor format probes"
)]
impl<'de> Visitor<'de> for RuntimeDescriptorFormatVisitor {
    type Value = RuntimeDescriptorFormat;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a runtime descriptor object with one numeric format field")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut descriptor_format = None;
        while let Some(field) = map.next_key::<String>()? {
            if field == "format" {
                if descriptor_format.is_some() {
                    return Err(A::Error::duplicate_field("format"));
                }
                descriptor_format = Some(map.next_value::<u64>()?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }

        descriptor_format
            .map(RuntimeDescriptorFormat)
            .ok_or_else(|| A::Error::missing_field("format"))
    }
}

#[expect(
    clippy::missing_trait_methods,
    reason = "serde's default deserialize_in_place implementation is sufficient"
)]
impl<'de> Deserialize<'de> for RuntimeDescriptorWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(RuntimeDescriptorWireVisitor)
    }
}

struct RuntimeDescriptorWireVisitor;

#[expect(
    clippy::missing_trait_methods,
    reason = "only JSON maps are valid runtime descriptors"
)]
impl<'de> Visitor<'de> for RuntimeDescriptorWireVisitor {
    type Value = RuntimeDescriptorWire;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a runtime descriptor object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut descriptor_format = None;
        let mut descriptor_entries = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "format" => {
                    if descriptor_format.is_some() {
                        return Err(A::Error::duplicate_field("format"));
                    }
                    descriptor_format = Some(map.next_value::<u8>()?);
                }
                "entries" => {
                    if descriptor_entries.is_some() {
                        return Err(A::Error::duplicate_field("entries"));
                    }
                    descriptor_entries = Some(map.next_value::<DescriptorEntries>()?.0);
                }
                _ => return Err(A::Error::unknown_field(&field, &["format", "entries"])),
            }
        }

        let parsed_format = descriptor_format.ok_or_else(|| A::Error::missing_field("format"))?;
        let parsed_entries =
            descriptor_entries.ok_or_else(|| A::Error::missing_field("entries"))?;
        Ok(RuntimeDescriptorWire {
            entries: parsed_entries,
            format: parsed_format,
        })
    }
}

struct DescriptorEntries(BTreeMap<String, String>);

#[expect(
    clippy::missing_trait_methods,
    reason = "serde's default deserialize_in_place implementation is sufficient"
)]
impl<'de> Deserialize<'de> for DescriptorEntries {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(DescriptorEntriesVisitor)
    }
}

struct DescriptorEntriesVisitor;

#[expect(
    clippy::missing_trait_methods,
    reason = "only JSON maps are valid runtime descriptor entries"
)]
impl<'de> Visitor<'de> for DescriptorEntriesVisitor {
    type Value = DescriptorEntries;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a map of runtime entry names to string values")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if entries.contains_key(&key) {
                return Err(A::Error::custom("duplicate runtime descriptor entry"));
            }
            let value = map.next_value::<String>()?;
            entries.insert(key, value);
        }
        Ok(DescriptorEntries(entries))
    }
}

#[derive(Clone, Copy)]
enum RuntimeValueKind {
    Boolean,
    IpAddress,
    LogLevel,
    NonblankPrintable,
    Port,
}

impl RuntimeValueKind {
    fn accepts(self, value: &str) -> bool {
        match self {
            Self::Boolean => value.parse::<bool>().is_ok(),
            Self::IpAddress => value.parse::<IpAddr>().is_ok(),
            Self::LogLevel => log::LevelFilter::from_str(value).is_ok(),
            Self::NonblankPrintable => is_nonblank_printable(value),
            Self::Port => value.parse::<u16>().is_ok_and(|port| port != 0),
        }
    }
}

#[cfg(feature = "cli")]
pub(crate) fn validated_deploy_env<I, K, V>(
    variables: I,
    config_ids: &[String],
    kv_ids: &[String],
    secret_ids: &[String],
) -> Result<EnvConfig, RuntimeDescriptorError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: Into<String>,
{
    let allowed_keys = runtime_value_kinds_for_ids(config_ids, kv_ids, secret_ids);
    let mut entries = BTreeMap::new();
    for (raw_key, value) in variables {
        let key_name = raw_key.as_ref();
        if allowed_keys.contains_key(key_name) {
            entries.insert(key_name.to_owned(), value.into());
        }
    }
    validated_env_from_entries(&entries, &allowed_keys)
}

fn validated_env_from_entries(
    entries: &BTreeMap<String, String>,
    allowed_keys: &BTreeMap<String, RuntimeValueKind>,
) -> Result<EnvConfig, RuntimeDescriptorError> {
    for (key, value) in entries {
        let Some(kind) = allowed_keys.get(key) else {
            return Err(RuntimeDescriptorError::UnsupportedEntry { key: key.clone() });
        };
        if !kind.accepts(value) {
            return Err(RuntimeDescriptorError::InvalidValue { key: key.clone() });
        }
    }
    Ok(EnvConfig::from_vars(entries.iter()))
}

#[cfg(any(feature = "cli", test))]
fn is_managed_store_name_key(key: &str) -> bool {
    let mut segments = key.split("__");
    matches!(segments.next(), Some("EDGEZERO"))
        && matches!(segments.next(), Some("STORES"))
        && matches!(segments.next(), Some("CONFIG" | "KV" | "SECRETS"))
        && segments.next().is_some_and(|id| !id.is_empty())
        && matches!(segments.next(), Some("NAME"))
        && segments.next().is_none()
}

fn is_nonblank_printable(value: &str) -> bool {
    !value.is_empty()
        && !value.chars().all(char::is_whitespace)
        && !value.chars().any(char::is_control)
}

#[must_use]
pub(crate) fn runtime_descriptor_key(service_id: &str, version: u64) -> String {
    format!("EDGEZERO__SERVICES__{service_id}__VERSIONS__{version}__ENV_V1")
}

#[cfg(any(feature = "fastly", test))]
pub(crate) fn runtime_env_config_from_lookup<E, F>(
    service_id: &str,
    version: u64,
    stores: StoresMetadata,
    mut lookup: F,
) -> Result<RuntimeEnvConfigResolution, RuntimeEnvConfigError>
where
    E: Error + Send + Sync + 'static,
    F: FnMut(&str) -> Result<Option<String>, RuntimeEnvLookupError<E>>,
{
    let descriptor_key = runtime_descriptor_key(service_id, version);
    let raw = match lookup(&descriptor_key) {
        Ok(Some(raw)) => raw,
        Ok(None) => {
            return missing_runtime_env_config(
                descriptor_key,
                stores,
                RuntimeEnvFallbackReason::DescriptorAbsent,
            );
        }
        Err(RuntimeEnvLookupError::SelectorStoreAbsent) => {
            return missing_runtime_env_config(
                descriptor_key,
                stores,
                RuntimeEnvFallbackReason::SelectorStoreAbsent,
            );
        }
        Err(RuntimeEnvLookupError::Lookup(source)) => {
            return Err(RuntimeEnvConfigError::LookupFailure {
                descriptor_key,
                source: Box::new(source),
            });
        }
    };
    let descriptor = RuntimeDescriptor::parse(&raw).map_err(|source| {
        RuntimeEnvConfigError::InvalidDescriptor {
            descriptor_key: descriptor_key.clone(),
            source,
        }
    })?;
    let env = descriptor.validated_env(stores).map_err(|source| {
        RuntimeEnvConfigError::InvalidDescriptor {
            descriptor_key,
            source,
        }
    })?;
    Ok(RuntimeEnvConfigResolution {
        env,
        fallback: None,
    })
}

#[cfg(any(feature = "fastly", test))]
fn missing_runtime_env_config(
    descriptor_key: String,
    stores: StoresMetadata,
    reason: RuntimeEnvFallbackReason,
) -> Result<RuntimeEnvConfigResolution, RuntimeEnvConfigError> {
    if stores.config.is_some() || stores.kv.is_some() || stores.secrets.is_some() {
        return Err(match reason {
            RuntimeEnvFallbackReason::DescriptorAbsent => {
                RuntimeEnvConfigError::DescriptorAbsent { descriptor_key }
            }
            RuntimeEnvFallbackReason::SelectorStoreAbsent => {
                RuntimeEnvConfigError::SelectorStoreAbsent { descriptor_key }
            }
        });
    }
    Ok(RuntimeEnvConfigResolution {
        env: EnvConfig::default(),
        fallback: Some(RuntimeEnvFallbackDiagnostic {
            descriptor_key,
            reason,
        }),
    })
}

fn runtime_value_kinds(stores: StoresMetadata) -> BTreeMap<String, RuntimeValueKind> {
    runtime_value_kinds_from_ids(
        stores
            .config
            .into_iter()
            .flat_map(|metadata| metadata.ids.iter().copied()),
        stores
            .kv
            .into_iter()
            .flat_map(|metadata| metadata.ids.iter().copied()),
        stores
            .secrets
            .into_iter()
            .flat_map(|metadata| metadata.ids.iter().copied()),
    )
}

#[cfg(feature = "cli")]
fn runtime_value_kinds_for_ids(
    config_ids: &[String],
    kv_ids: &[String],
    secret_ids: &[String],
) -> BTreeMap<String, RuntimeValueKind> {
    runtime_value_kinds_from_ids(
        config_ids.iter().map(String::as_str),
        kv_ids.iter().map(String::as_str),
        secret_ids.iter().map(String::as_str),
    )
}

fn runtime_value_kinds_from_ids<'ids, C, K, S>(
    config_ids: C,
    kv_ids: K,
    secret_ids: S,
) -> BTreeMap<String, RuntimeValueKind>
where
    C: IntoIterator<Item = &'ids str>,
    K: IntoIterator<Item = &'ids str>,
    S: IntoIterator<Item = &'ids str>,
{
    let mut allowed_keys = FIXED_RUNTIME_KEYS
        .into_iter()
        .map(|(key, kind)| (key.to_owned(), kind))
        .collect::<BTreeMap<_, _>>();
    add_runtime_store_value_kinds(&mut allowed_keys, "CONFIG", config_ids);
    add_runtime_store_value_kinds(&mut allowed_keys, "KV", kv_ids);
    add_runtime_store_value_kinds(&mut allowed_keys, "SECRETS", secret_ids);
    allowed_keys
}

fn add_runtime_store_value_kinds<'ids, I>(
    allowed_keys: &mut BTreeMap<String, RuntimeValueKind>,
    kind: &str,
    ids: I,
) where
    I: IntoIterator<Item = &'ids str>,
{
    for store_id in ids {
        let canonical_id = store_id.to_ascii_uppercase();
        allowed_keys.insert(
            format!("EDGEZERO__STORES__{kind}__{canonical_id}__NAME"),
            RuntimeValueKind::NonblankPrintable,
        );
        if kind == "CONFIG" {
            allowed_keys.insert(
                format!("EDGEZERO__STORES__{kind}__{canonical_id}__KEY"),
                RuntimeValueKind::NonblankPrintable,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
    use edgezero_core::app::{StoreMetadata, StoresMetadata};
    use std::collections::{BTreeMap, BTreeSet};
    use std::io;
    use std::slice;

    fn stores() -> StoresMetadata {
        StoresMetadata {
            config: Some(StoreMetadata {
                default: "app_config",
                ids: &["app_config"],
            }),
            kv: Some(StoreMetadata {
                default: "sessions",
                ids: &["sessions"],
            }),
            secrets: Some(StoreMetadata {
                default: "credentials",
                ids: &["credentials"],
            }),
        }
    }

    fn descriptor(
        entries: impl IntoIterator<Item = (&'static str, &'static str)>,
    ) -> RuntimeDescriptor {
        RuntimeDescriptor::from_entries(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        )
        .expect("test descriptor must fit")
    }

    fn descriptor_json(entries: impl IntoIterator<Item = (&'static str, &'static str)>) -> String {
        descriptor(entries).canonical_json().unwrap()
    }

    #[test]
    fn descriptor_lookup_scopes_shared_store_by_service() {
        let first_key = runtime_descriptor_key("SvcA1", 42);
        let second_key = runtime_descriptor_key("SvcB2", 42);
        let values = BTreeMap::from([
            (
                first_key.clone(),
                descriptor_json([("EDGEZERO__ADAPTER__PORT", "4101")]),
            ),
            (
                second_key.clone(),
                descriptor_json([("EDGEZERO__ADAPTER__PORT", "4102")]),
            ),
        ]);
        let mut lookups = Vec::new();

        let first = runtime_env_config_from_lookup("SvcA1", 42, StoresMetadata::default(), |key| {
            lookups.push(key.to_owned());
            Ok::<_, RuntimeEnvLookupError<io::Error>>(values.get(key).cloned())
        })
        .unwrap();
        let second =
            runtime_env_config_from_lookup("SvcB2", 42, StoresMetadata::default(), |key| {
                lookups.push(key.to_owned());
                Ok::<_, RuntimeEnvLookupError<io::Error>>(values.get(key).cloned())
            })
            .unwrap();

        assert_eq!(first.env.adapter_port(), Some("4101"));
        assert_eq!(second.env.adapter_port(), Some("4102"));
        assert_eq!(lookups, [first_key, second_key]);
    }

    #[test]
    fn descriptor_lookup_scopes_shared_store_by_version() {
        let old_key = runtime_descriptor_key("SvcA1", 42);
        let new_key = runtime_descriptor_key("SvcA1", 43);
        let values = BTreeMap::from([
            (
                old_key.clone(),
                descriptor_json([("EDGEZERO__LOGGING__LEVEL", "info")]),
            ),
            (
                new_key.clone(),
                descriptor_json([("EDGEZERO__LOGGING__LEVEL", "debug")]),
            ),
        ]);
        let mut lookups = Vec::new();

        let old = runtime_env_config_from_lookup("SvcA1", 42, StoresMetadata::default(), |key| {
            lookups.push(key.to_owned());
            Ok::<_, RuntimeEnvLookupError<io::Error>>(values.get(key).cloned())
        })
        .unwrap();
        let new = runtime_env_config_from_lookup("SvcA1", 43, StoresMetadata::default(), |key| {
            lookups.push(key.to_owned());
            Ok::<_, RuntimeEnvLookupError<io::Error>>(values.get(key).cloned())
        })
        .unwrap();

        assert_eq!(old.env.logging_level(), Some("info"));
        assert_eq!(new.env.logging_level(), Some("debug"));
        assert_eq!(lookups, [old_key, new_key]);
    }

    #[test]
    fn descriptor_lookup_allows_store_free_missing_store_or_descriptor() {
        const SENSITIVE: &str = "SENSITIVE_RAW_DESCRIPTOR_VALUE";
        let expected_key = runtime_descriptor_key("SvcA1", 42);
        let legacy_values =
            BTreeMap::from([("EDGEZERO__ADAPTER__PORT".to_owned(), SENSITIVE.to_owned())]);
        let mut missing_store_lookups = Vec::new();
        let mut missing_descriptor_lookups = Vec::new();

        let missing_store =
            runtime_env_config_from_lookup("SvcA1", 42, StoresMetadata::default(), |key| {
                missing_store_lookups.push(key.to_owned());
                Err::<Option<String>, _>(RuntimeEnvLookupError::<io::Error>::SelectorStoreAbsent)
            })
            .expect("store-free app may use defaults without selector store");
        let missing_descriptor =
            runtime_env_config_from_lookup("SvcA1", 42, StoresMetadata::default(), |key| {
                missing_descriptor_lookups.push(key.to_owned());
                Ok::<_, RuntimeEnvLookupError<io::Error>>(legacy_values.get(key).cloned())
            })
            .expect("store-free app may use defaults without descriptor");

        assert_eq!(
            missing_store_lookups.as_slice(),
            slice::from_ref(&expected_key)
        );
        assert_eq!(
            missing_descriptor_lookups.as_slice(),
            slice::from_ref(&expected_key)
        );
        for resolution in [missing_store, missing_descriptor] {
            assert_eq!(resolution.env, EnvConfig::default());
            let diagnostic = resolution
                .fallback
                .expect("fallback must carry an operator diagnostic")
                .to_string();
            assert!(diagnostic.contains(&expected_key), "{diagnostic}");
            assert!(!diagnostic.contains(SENSITIVE), "{diagnostic}");
        }
    }

    #[test]
    fn descriptor_lookup_error_is_error_send_sync_and_static() {
        fn assert_error<E: Error + Send + Sync + 'static>() {}

        assert_error::<RuntimeEnvConfigError>();
    }

    #[test]
    fn descriptor_lookup_rejects_missing_store_or_descriptor_for_every_store_kind() {
        const IDS: &[&str] = &["main"];
        let metadata = StoreMetadata {
            default: "main",
            ids: IDS,
        };
        let store_cases = [
            StoresMetadata {
                config: Some(metadata),
                ..StoresMetadata::default()
            },
            StoresMetadata {
                kv: Some(metadata),
                ..StoresMetadata::default()
            },
            StoresMetadata {
                secrets: Some(metadata),
                ..StoresMetadata::default()
            },
        ];

        for stores in store_cases {
            let missing_store = runtime_env_config_from_lookup("SvcA1", 42, stores, |_key| {
                Err::<Option<String>, _>(RuntimeEnvLookupError::<io::Error>::SelectorStoreAbsent)
            })
            .expect_err("a store-declaring app requires the selector store");
            assert!(matches!(
                missing_store,
                RuntimeEnvConfigError::SelectorStoreAbsent { .. }
            ));

            let missing_descriptor = runtime_env_config_from_lookup("SvcA1", 42, stores, |_key| {
                Ok::<_, RuntimeEnvLookupError<io::Error>>(None)
            })
            .expect_err("a store-declaring app requires a descriptor");
            assert!(matches!(
                missing_descriptor,
                RuntimeEnvConfigError::DescriptorAbsent { .. }
            ));
        }
    }

    #[test]
    fn descriptor_lookup_errors_fail_without_exposing_provider_details() {
        let error =
            runtime_env_config_from_lookup("SvcA1", 42, StoresMetadata::default(), |_key| {
                Err(RuntimeEnvLookupError::Lookup(io::Error::other(
                    "SENSITIVE_PROVIDER_DETAIL",
                )))
            })
            .expect_err("lookup failures never fall back");

        assert!(matches!(error, RuntimeEnvConfigError::LookupFailure { .. }));
        assert!(!error.to_string().contains("SENSITIVE_PROVIDER_DETAIL"));
    }

    #[test]
    fn descriptor_lookup_rejects_malformed_unsupported_and_invalid_descriptors() {
        let cases = [
            ("malformed", "SENSITIVE_NOT_JSON".to_owned()),
            (
                "unsupported",
                r#"{"format":2,"entries":{"future":"SENSITIVE_FUTURE"}}"#.to_owned(),
            ),
            (
                "invalid value",
                descriptor_json([("EDGEZERO__ADAPTER__PORT", "SENSITIVE_INVALID_PORT")]),
            ),
        ];

        for (case, raw) in cases {
            let error =
                runtime_env_config_from_lookup("SvcA1", 42, StoresMetadata::default(), |_key| {
                    Ok::<_, RuntimeEnvLookupError<io::Error>>(Some(raw.clone()))
                })
                .expect_err(case);

            assert!(matches!(
                error,
                RuntimeEnvConfigError::InvalidDescriptor { .. }
            ));
            let diagnostic = error.to_string();
            assert!(!diagnostic.contains("SENSITIVE"), "{case}: {diagnostic}");
            if case == "unsupported" {
                assert!(diagnostic.contains("upgrade"), "{diagnostic}");
            }
        }
    }

    #[test]
    fn runtime_descriptor_keys_are_scoped_by_service_and_version() {
        assert_eq!(
            runtime_descriptor_key("SvcA1", 42),
            "EDGEZERO__SERVICES__SvcA1__VERSIONS__42__ENV_V1"
        );
        assert_eq!(
            runtime_descriptor_key("SvcB2", 42),
            "EDGEZERO__SERVICES__SvcB2__VERSIONS__42__ENV_V1"
        );
        assert_eq!(
            runtime_descriptor_key("SvcA1", 43),
            "EDGEZERO__SERVICES__SvcA1__VERSIONS__43__ENV_V1"
        );
    }

    #[cfg(feature = "cli")]
    #[test]
    fn deploy_plan_descriptor_captures_fixed_values_and_all_store_selections() {
        let environment = EnvConfig::from_vars([
            ("EDGEZERO__ADAPTER__PORT", "8080"),
            ("EDGEZERO__LOGGING__LEVEL", "debug"),
            ("EDGEZERO__STORES__CONFIG__APP__NAME", "app-prod"),
            ("EDGEZERO__STORES__CONFIG__APP__KEY", "publisher-a"),
            ("EDGEZERO__STORES__KV__CACHE__NAME", "cache-prod"),
            ("EDGEZERO__STORES__SECRETS__TOKEN__NAME", "token-prod"),
        ]);
        let descriptor = RuntimeDescriptor::from_environment(
            &environment,
            &["app".to_owned()],
            &["cache".to_owned()],
            &["token".to_owned()],
        )
        .expect("descriptor");
        assert_eq!(
            descriptor.entries,
            BTreeMap::from([
                ("EDGEZERO__ADAPTER__PORT".to_owned(), "8080".to_owned()),
                ("EDGEZERO__LOGGING__LEVEL".to_owned(), "debug".to_owned(),),
                (
                    "EDGEZERO__STORES__CONFIG__APP__KEY".to_owned(),
                    "publisher-a".to_owned(),
                ),
                (
                    "EDGEZERO__STORES__CONFIG__APP__NAME".to_owned(),
                    "app-prod".to_owned(),
                ),
                (
                    "EDGEZERO__STORES__KV__CACHE__NAME".to_owned(),
                    "cache-prod".to_owned(),
                ),
                (
                    "EDGEZERO__STORES__SECRETS__TOKEN__NAME".to_owned(),
                    "token-prod".to_owned(),
                ),
            ])
        );
    }

    #[test]
    fn canonical_json_sorts_entries_and_round_trips() {
        let descriptor = descriptor([
            ("EDGEZERO__LOGGING__LEVEL", "debug"),
            ("EDGEZERO__ADAPTER__PORT", "443"),
            ("EDGEZERO__ADAPTER__HOST", "127.0.0.1"),
        ]);

        let json = descriptor.canonical_json().expect("serialize descriptor");

        assert_eq!(
            json.as_bytes(),
            br#"{"format":1,"entries":{"EDGEZERO__ADAPTER__HOST":"127.0.0.1","EDGEZERO__ADAPTER__PORT":"443","EDGEZERO__LOGGING__LEVEL":"debug"}}"#
        );
        let reparsed = RuntimeDescriptor::parse(&json).expect("parse canonical descriptor");
        assert_eq!(reparsed.canonical_json().unwrap(), json);
    }

    #[test]
    fn validated_env_accepts_fixed_values_and_declared_store_selectors() {
        let descriptor = descriptor([
            ("EDGEZERO__ADAPTER__HOST", "::1"),
            ("EDGEZERO__ADAPTER__PORT", "8443"),
            ("EDGEZERO__LOGGING__LEVEL", "warn"),
            ("EDGEZERO__LOGGING__ENDPOINT", "edgezero.logs"),
            ("EDGEZERO__LOGGING__USE_FASTLY_LOGGER", "true"),
            ("EDGEZERO__LOGGING__ECHO_STDOUT", "false"),
            ("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME", "prod-config"),
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY",
                "prod_config_v2",
            ),
            ("EDGEZERO__STORES__KV__SESSIONS__NAME", "prod-sessions"),
            (
                "EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME",
                "prod-secrets",
            ),
        ]);

        let canonical = descriptor
            .canonical_json()
            .expect("serialize valid descriptor");
        let parsed = RuntimeDescriptor::parse(&canonical).expect("parse valid descriptor");
        let env = parsed.validated_env(stores()).expect("valid descriptor");

        assert_eq!(env.adapter_host(), Some("::1"));
        assert_eq!(env.adapter_port(), Some("8443"));
        assert_eq!(env.logging_level(), Some("warn"));
        assert_eq!(env.logging_endpoint(), Some("edgezero.logs"));
        assert_eq!(env.get(&["logging", "use_fastly_logger"]), Some("true"));
        assert_eq!(env.get(&["logging", "echo_stdout"]), Some("false"));
        assert_eq!(env.store_name("config", "app_config"), "prod-config");
        assert_eq!(env.store_key("config", "app_config"), "prod_config_v2");
        assert_eq!(env.store_name("kv", "sessions"), "prod-sessions");
        assert_eq!(env.store_name("secrets", "credentials"), "prod-secrets");
    }

    #[test]
    fn managed_store_aliases_find_prior_physical_store_without_current_metadata() {
        let descriptor = descriptor([
            ("EDGEZERO__ADAPTER__PORT", "443"),
            (
                "EDGEZERO__STORES__CONFIG__REMOVED_STORE__NAME",
                "old-physical-config",
            ),
            ("EDGEZERO__STORES__CONFIG__REMOVED_STORE__KEY", "old-key"),
        ]);

        assert_eq!(
            descriptor.managed_store_aliases().unwrap(),
            BTreeSet::from(["old-physical-config".to_owned()])
        );
    }

    #[test]
    fn parse_rejects_malformed_descriptor_shapes() {
        let cases = [
            (
                "duplicate format",
                r#"{"format":1,"format":1,"entries":{}}"#,
            ),
            (
                "duplicate entries",
                r#"{"format":1,"entries":{},"entries":{}}"#,
            ),
            (
                "duplicate entry name",
                r#"{"format":1,"entries":{"EDGEZERO__ADAPTER__PORT":"1","EDGEZERO__ADAPTER__PORT":"2"}}"#,
            ),
            ("missing both fields", "{}"),
            ("missing entries", r#"{"format":1}"#),
            ("missing format", r#"{"entries":{}}"#),
            (
                "unknown top-level field",
                r#"{"format":1,"entries":{},"extra":"SENSITIVE_UNKNOWN"}"#,
            ),
            ("top-level array", "[]"),
            ("format wrong type", r#"{"format":"1","entries":{}}"#),
            ("entries wrong type", r#"{"format":1,"entries":[]}"#),
            (
                "entry value wrong type",
                r#"{"format":1,"entries":{"EDGEZERO__ADAPTER__PORT":443}}"#,
            ),
            ("unsupported format", r#"{"format":2,"entries":{}}"#),
        ];

        for (case, raw) in cases {
            let error = RuntimeDescriptor::parse(raw).expect_err(case).to_string();
            assert!(!error.contains("SENSITIVE_UNKNOWN"), "{case}: {error}");
        }
    }

    #[test]
    fn parse_rejects_raw_input_over_fastly_entry_limit_before_json_parsing() {
        let error = RuntimeDescriptor::parse(&"S".repeat(FASTLY_CONFIG_ENTRY_LIMIT + 1))
            .expect_err("oversized raw input must fail before malformed JSON classification");

        assert!(matches!(error, RuntimeDescriptorError::EntryTooLarge));
        assert!(!error.to_string().contains(&"S".repeat(128)));
    }

    #[test]
    fn parse_reports_unsupported_format_as_upgrade_requirement() {
        let cases = [
            (
                "entries array",
                r#"{"format":2,"entries":[{"future":"SENSITIVE_ARRAY"}]}"#,
                2_u64,
            ),
            (
                "new entry shape",
                r#"{"format":2,"entries":{"future":{"nested":"SENSITIVE_NESTED"}}}"#,
                2,
            ),
            (
                "extra top-level field",
                r#"{"format":2,"entries":{},"future":{"value":"SENSITIVE_EXTRA"}}"#,
                2,
            ),
            (
                "format after entries",
                r#"{"entries":{"future":{"value":"SENSITIVE_LATE"}},"format":2}"#,
                2,
            ),
            (
                "format larger than u8",
                r#"{"format":256,"entries":{"future":"SENSITIVE_WIDE"}}"#,
                256,
            ),
        ];

        for (case, raw, expected_format) in cases {
            let error = RuntimeDescriptor::parse(raw).expect_err(case);
            let RuntimeDescriptorError::UnsupportedFormat { format } = error else {
                panic!("{case} must be classified as an unsupported format");
            };
            assert_eq!(format, expected_format, "{case}");
            let diagnostic = error.to_string();
            assert!(diagnostic.contains("upgrade"), "{case}: {diagnostic}");
            assert!(
                diagnostic.contains(&expected_format.to_string()),
                "{case}: {diagnostic}"
            );
            assert!(!diagnostic.contains("SENSITIVE"), "{case}: {diagnostic}");
        }
    }

    #[test]
    fn parse_preserves_duplicate_format_as_invalid_json() {
        let error =
            RuntimeDescriptor::parse(r#"{"format":2,"entries":{"future":true},"format":3}"#)
                .expect_err("duplicate format must remain structurally invalid");

        assert!(matches!(error, RuntimeDescriptorError::InvalidJson));
    }

    #[test]
    fn validated_env_rejects_unknown_and_undeclared_keys() {
        let cases = [
            "EDGEZERO__UNKNOWN__SETTING",
            "EDGEZERO__STORES__KV__UNDECLARED__NAME",
            "EDGEZERO__STORES__KV__SESSIONS__KEY",
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__TTL",
        ];

        for key in cases {
            let descriptor = RuntimeDescriptor::from_entries(BTreeMap::from([(
                key.to_owned(),
                "SENSITIVE_UNKNOWN_VALUE".to_owned(),
            )]))
            .unwrap();
            let error = descriptor
                .validated_env(stores())
                .expect_err(key)
                .to_string();
            assert!(
                error.contains(key),
                "diagnostic should identify {key}: {error}"
            );
            assert!(!error.contains("SENSITIVE_UNKNOWN_VALUE"), "{key}: {error}");
        }
    }

    #[test]
    fn entry_error_diagnostics_escape_control_characters_in_keys() {
        let injected_key = "EDGEZERO__UNKNOWN\nINJECTED\u{0007}";
        let descriptor = RuntimeDescriptor::from_entries(BTreeMap::from([(
            injected_key.to_owned(),
            "SENSITIVE_UNKNOWN_VALUE".to_owned(),
        )]))
        .unwrap();
        let unsupported = descriptor
            .validated_env(stores())
            .expect_err("unknown control-bearing key must fail");
        let invalid = RuntimeDescriptorError::InvalidValue {
            key: injected_key.to_owned(),
        };

        for diagnostic in [unsupported.to_string(), invalid.to_string()] {
            assert!(diagnostic.contains("\\n"));
            assert!(diagnostic.contains("\\u{7}"));
            assert!(!diagnostic.chars().any(char::is_control));
            assert!(!diagnostic.contains("SENSITIVE_UNKNOWN_VALUE"));
        }
    }

    #[test]
    fn validated_env_rejects_invalid_values_without_echoing_them() {
        let cases = [
            ("EDGEZERO__ADAPTER__HOST", "SENSITIVE_INVALID_HOST"),
            ("EDGEZERO__ADAPTER__PORT", "SENSITIVE_INVALID_PORT"),
            ("EDGEZERO__ADAPTER__PORT", "0"),
            (
                "EDGEZERO__LOGGING__USE_FASTLY_LOGGER",
                "SENSITIVE_INVALID_BOOL",
            ),
            ("EDGEZERO__LOGGING__ECHO_STDOUT", "yes"),
            ("EDGEZERO__LOGGING__LEVEL", "SENSITIVE_INVALID_LEVEL"),
            ("EDGEZERO__LOGGING__ENDPOINT", " \t "),
            ("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME", "   "),
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME",
                "SENSITIVE\nSTORE",
            ),
            ("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY", "   "),
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY",
                "SENSITIVE\u{0007}KEY",
            ),
            ("EDGEZERO__STORES__KV__SESSIONS__NAME", "SENSITIVE\0STORE"),
            ("EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME", "\u{0007}"),
        ];

        for (key, raw_value) in cases {
            let descriptor = RuntimeDescriptor::from_entries(BTreeMap::from([(
                key.to_owned(),
                raw_value.to_owned(),
            )]))
            .unwrap();
            let error = descriptor
                .validated_env(stores())
                .expect_err(key)
                .to_string();
            assert!(
                error.contains(key),
                "diagnostic should identify {key}: {error}"
            );
            assert!(
                !error.contains(raw_value),
                "diagnostic leaked value for {key}"
            );
            assert!(
                !error.contains("SENSITIVE"),
                "diagnostic leaked value: {error}"
            );
        }
    }

    #[test]
    fn all_error_diagnostics_redact_raw_values() {
        let sensitive = "TOP_SECRET_VALUE_9dfbb7";
        let invalid_descriptor = descriptor([("EDGEZERO__LOGGING__LEVEL", sensitive)]);
        let validation_error = invalid_descriptor
            .validated_env(stores())
            .unwrap_err()
            .to_string();
        assert!(!validation_error.contains(sensitive));

        let alias_descriptor = descriptor([(
            "EDGEZERO__STORES__KV__OLD__NAME",
            "TOP_SECRET_VALUE_9dfbb7\n",
        )]);
        let alias_error = alias_descriptor
            .managed_store_aliases()
            .unwrap_err()
            .to_string();
        assert!(!alias_error.contains(sensitive));
    }

    #[test]
    fn alias_discovery_accepts_exact_name_shape_and_ignores_everything_else() {
        let descriptor = descriptor([
            ("EDGEZERO__STORES__CONFIG__OLD__NAME", "old-config"),
            ("EDGEZERO__STORES__KV__CACHE__NAME", "old-kv"),
            ("EDGEZERO__STORES__SECRETS__VAULT__NAME", "old-secrets"),
            ("EDGEZERO__STORES__CONFIG__OLD__KEY", "not-an-alias"),
            ("EDGEZERO__STORES__BLOB__OLD__NAME", "not-an-alias"),
            ("EDGEZERO__STORES__KV____NAME", "not-an-alias"),
            ("EDGEZERO__STORES__KV__OLD__NAME__EXTRA", "not-an-alias"),
            ("EDGEZERO__LOGGING__ENDPOINT", "not-an-alias"),
            ("ARBITRARY", "not-an-alias"),
        ]);

        assert_eq!(
            descriptor.managed_store_aliases().unwrap(),
            BTreeSet::from([
                "old-config".to_owned(),
                "old-kv".to_owned(),
                "old-secrets".to_owned(),
            ])
        );
    }

    #[test]
    fn runtime_validation_and_alias_discovery_apply_separate_policies() {
        let descriptor = descriptor([
            ("EDGEZERO__STORES__KV__OLD__NAME", "old-kv"),
            ("EDGEZERO__STORES__KV__OLD__TTL", "SENSITIVE_UNKNOWN_SHAPE"),
        ]);

        let runtime_error = descriptor
            .validated_env(StoresMetadata::default())
            .expect_err("runtime must reject undeclared and unknown entry shapes")
            .to_string();
        assert!(!runtime_error.contains("SENSITIVE_UNKNOWN_SHAPE"));
        assert_eq!(
            descriptor.managed_store_aliases().unwrap(),
            BTreeSet::from(["old-kv".to_owned()])
        );
    }

    #[test]
    fn serialization_enforces_exact_fastly_entry_byte_boundary() {
        const KEY: &str = "EDGEZERO__LOGGING__ENDPOINT";

        let empty = RuntimeDescriptor {
            entries: BTreeMap::from([(KEY.to_owned(), String::new())]),
        };
        let serialized_overhead = serde_json::to_string(&empty).unwrap().len();
        let exact_entries = BTreeMap::from([(
            KEY.to_owned(),
            "x".repeat(FASTLY_CONFIG_ENTRY_LIMIT - serialized_overhead),
        )]);

        let exact = RuntimeDescriptor::from_entries(exact_entries.clone())
            .expect("an exactly 8,000-byte descriptor must fit");
        let exact_json = exact.canonical_json().expect("serialize exact boundary");
        assert_eq!(exact_json.len(), FASTLY_CONFIG_ENTRY_LIMIT);
        let parsed = RuntimeDescriptor::parse(&exact_json)
            .expect("an exactly 8,000-byte canonical descriptor must parse");
        parsed
            .validated_env(StoresMetadata::default())
            .expect("the boundary descriptor must otherwise be valid");

        let mut oversized_entries = exact_entries;
        oversized_entries
            .get_mut(KEY)
            .expect("endpoint entry")
            .push('x');
        let oversized = RuntimeDescriptor {
            entries: oversized_entries.clone(),
        };
        assert_eq!(
            serde_json::to_string(&oversized).unwrap().len(),
            FASTLY_CONFIG_ENTRY_LIMIT + 1
        );
        assert!(matches!(
            oversized.canonical_json(),
            Err(RuntimeDescriptorError::EntryTooLarge)
        ));

        let error = RuntimeDescriptor::from_entries(oversized_entries)
            .expect_err("an exactly 8,001-byte descriptor must be rejected");
        assert!(matches!(error, RuntimeDescriptorError::EntryTooLarge));
        assert!(!error.to_string().contains(&"x".repeat(128)));
    }
}
