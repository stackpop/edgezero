//! SDK-free typed settings and validation. Resource values never enter diagnostics.

#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "settings, validators and their tests are grouped by provider"
)]

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use percent_encoding::percent_decode_str;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use url::Url;

use crate::PreparationError;

// Portable SecretHandle limits names to 512 UTF-8 bytes. Keep SDK-free settings
// independent of core; service implementations must use the same core bound.
const MAX_SECRET_KEY_BYTES: usize = 512;

/// One configuration key maps to one complete free-form Agent document.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDocument {
    pub application: String,
    pub environment: String,
    pub profile: String,
}

impl fmt::Debug for AgentDocument {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentDocument").finish_non_exhaustive()
    }
}

/// `AppConfig` Agent settings. The token field names a bootstrap variable, not a value.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfigAgentSettings {
    pub default_key: String,
    pub endpoint: String,
    pub access_token_env: Option<String>,
    pub documents: BTreeMap<String, AgentDocument>,
}

impl fmt::Debug for AppConfigAgentSettings {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfigAgentSettings")
            .finish_non_exhaustive()
    }
}

impl AppConfigAgentSettings {
    /// Builds an exact Agent request URL using checked path segments.
    ///
    /// # Errors
    /// Rejects ambiguous/non-loopback endpoints and unsafe document identifiers.
    #[inline]
    pub fn document_url(&self, document: &AgentDocument) -> Result<Url, PreparationError> {
        let mut url = agent_endpoint(&self.endpoint)?;
        let expected = [
            "applications",
            document.application.as_str(),
            "environments",
            document.environment.as_str(),
            "configurations",
            document.profile.as_str(),
        ];
        for segment in [
            &document.application,
            &document.environment,
            &document.profile,
        ] {
            if !valid_agent_segment(segment) {
                return Err(PreparationError::InvalidResource);
            }
        }
        url.path_segments_mut()
            .map_err(|_error| PreparationError::InvalidEndpoint)?
            .clear()
            .extend(expected);
        let reparsed =
            Url::parse(url.as_str()).map_err(|_error| PreparationError::InvalidEndpoint)?;
        if !reparsed.path_segments().is_some_and(|segments| {
            segments
                .map(|segment| percent_decode_str(segment).decode_utf8())
                .eq(expected.into_iter().map(|segment| Ok(segment.into())))
        }) {
            return Err(PreparationError::InvalidResource);
        }
        Ok(reparsed)
    }

    /// Validates every mapping without HTTP or bootstrap environment access.
    ///
    /// # Errors
    /// Returns a closed category for invalid endpoint, mapping, default or token name.
    #[inline]
    pub fn validate(&self) -> Result<(), PreparationError> {
        agent_endpoint(&self.endpoint)?;
        if self.documents.is_empty()
            || !self.documents.contains_key(&self.default_key)
            || self.documents.keys().any(|key| !valid_lookup_key(key))
            || self
                .access_token_env
                .as_deref()
                .is_some_and(|name| !valid_variable_name(name))
        {
            return Err(PreparationError::InvalidBinding);
        }
        for document in self.documents.values() {
            self.document_url(document)?;
        }
        Ok(())
    }
}

/// A bootstrap token. Debug hides both the token and its length.
#[derive(Clone)]
pub struct AgentAccessToken(String);

impl AgentAccessToken {
    /// Captures an explicit nonempty bearer token without reading the environment.
    ///
    /// # Errors
    /// Rejects missing/empty tokens and bytes that cannot be sent as a bearer value.
    #[inline]
    pub fn new(value: String) -> Result<Self, PreparationError> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(PreparationError::InvalidBinding);
        }
        Ok(Self(value))
    }

    /// Exposes the token only to the provider transport.
    #[inline]
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AgentAccessToken {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AgentAccessToken([redacted])")
    }
}

/// Exactly one version selector. Omission means AWSCURRENT at startup.
/// API lengths count characters; non-control Unicode and embedded spaces retain
/// their exact meaning. Validation does not assert that a version/stage exists.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub enum SecretVersion {
    Current,
    Stage(String),
    VersionId(String),
}

impl SecretVersion {
    fn validate(&self) -> Result<(), PreparationError> {
        let (value, minimum, maximum) = match self {
            Self::Current => return Ok(()),
            Self::Stage(value) => (value, 1, 256),
            Self::VersionId(value) => (value, 32, 64),
        };
        let length = value.chars().count();
        if !(minimum..=maximum).contains(&length)
            || value.chars().all(char::is_whitespace)
            || value.chars().any(char::is_control)
        {
            return Err(PreparationError::InvalidSelector);
        }
        Ok(())
    }
}

impl fmt::Debug for SecretVersion {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretVersion([redacted])")
    }
}

/// A complete ARN and a validated selector. Construction never discovers credentials.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct SecretTarget {
    secret_id: String,
    version: SecretVersion,
}

impl SecretTarget {
    /// Validates a complete ARN and mutually exclusive typed selector.
    ///
    /// # Errors
    /// Rejects incomplete/unsupported resources and invalid selector lengths.
    #[inline]
    pub fn new(secret_id: String, version: SecretVersion) -> Result<Self, PreparationError> {
        secret_region(&secret_id)?;
        version.validate()?;
        let selected = match version {
            SecretVersion::Stage(stage) if stage == "AWSCURRENT" => SecretVersion::Current,
            other @ (SecretVersion::Current
            | SecretVersion::Stage(_)
            | SecretVersion::VersionId(_)) => other,
        };
        Ok(Self {
            secret_id,
            version: selected,
        })
    }

    /// Complete resource identifier for the service request, never diagnostics.
    #[inline]
    #[must_use]
    pub fn secret_id(&self) -> &str {
        &self.secret_id
    }

    /// Exact version selection for the service request.
    #[inline]
    #[must_use]
    pub fn version(&self) -> &SecretVersion {
        &self.version
    }
}

impl fmt::Debug for SecretTarget {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretTarget").finish_non_exhaustive()
    }
}

impl<'de> Deserialize<'de> for SecretTarget {
    #[inline]
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            secret_id: String,
            version_id: Option<String>,
            version_stage: Option<String>,
        }
        let input = Input::deserialize(deserializer)?;
        let version = match (input.version_id, input.version_stage) {
            (None, None) => SecretVersion::Current,
            (None, Some(stage)) => SecretVersion::Stage(stage),
            (Some(id), None) => SecretVersion::VersionId(id),
            (Some(_), Some(_)) => {
                return Err(D::Error::custom(PreparationError::InvalidSelector));
            }
        };
        Self::new(input.secret_id, version).map_err(D::Error::custom)
    }

    #[inline]
    fn deserialize_in_place<D: Deserializer<'de>>(
        deserializer: D,
        place: &mut Self,
    ) -> Result<(), D::Error> {
        *place = Self::deserialize(deserializer)?;
        Ok(())
    }
}

/// Explicit region and finite secret key mappings. Namespaces are supplied by the host.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsManagerSettings {
    pub region: String,
    pub entries: BTreeMap<String, SecretTarget>,
}

impl SecretsManagerSettings {
    /// Validates explicit region, keys and every ARN's region/partition relationship.
    ///
    /// # Errors
    /// Rejects empty mappings, invalid keys, missing regions and mismatched ARNs.
    #[inline]
    pub fn validate(&self) -> Result<(), PreparationError> {
        // Match the portable SecretHandle byte limit without adding core to the
        // settings-only graph. Provider code must also enforce core's bound.
        if self.entries.is_empty()
            || self
                .entries
                .keys()
                .any(|key| !valid_lookup_key(key) || key.len() > MAX_SECRET_KEY_BYTES)
        {
            return Err(PreparationError::InvalidBinding);
        }
        for target in self.entries.values() {
            if secret_region(target.secret_id())? != self.region {
                return Err(PreparationError::InvalidResource);
            }
        }
        Ok(())
    }
}

impl fmt::Debug for SecretsManagerSettings {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretsManagerSettings")
            .finish_non_exhaustive()
    }
}

/// Remote preparation policy for one explicitly shared session, not a process/RSS cap.
/// Omitted fields use documented defaults from this single policy source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AwsPreparationLimits {
    pub max_remote_values: u64,
    pub max_config_document_bytes: u64,
    pub max_secret_value_bytes: u64,
    pub max_snapshot_bytes: u64,
    pub preparation_timeout_ms: u64,
    pub operation_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub attempt_timeout_ms: u64,
}

impl Default for AwsPreparationLimits {
    #[inline]
    fn default() -> Self {
        Self {
            max_remote_values: 1_024,
            max_config_document_bytes: 1_024 * 1_024,
            max_secret_value_bytes: 64 * 1_024,
            max_snapshot_bytes: 8 * 1_024 * 1_024,
            preparation_timeout_ms: 30_000,
            operation_timeout_ms: 5_000,
            connect_timeout_ms: 1_000,
            attempt_timeout_ms: 2_000,
        }
    }
}

impl AwsPreparationLimits {
    /// Validates finite representable bounds and consistency, without hardcoding the defaults as maxima.
    ///
    /// # Errors
    /// Rejects zero, unusable timer/counter values and inconsistent limits.
    #[inline]
    pub fn validate(&self) -> Result<(), PreparationError> {
        for value in [
            self.max_remote_values,
            self.max_config_document_bytes,
            self.max_secret_value_bytes,
            self.max_snapshot_bytes,
        ] {
            if value == 0 || usize::try_from(value).is_err() {
                return Err(PreparationError::InvalidLimits);
            }
        }
        for millis in [
            self.preparation_timeout_ms,
            self.operation_timeout_ms,
            self.connect_timeout_ms,
            self.attempt_timeout_ms,
        ] {
            if millis == 0
                || Instant::now()
                    .checked_add(Duration::from_millis(millis))
                    .is_none()
            {
                return Err(PreparationError::InvalidLimits);
            }
        }
        if self.max_config_document_bytes > self.max_snapshot_bytes
            || self.max_secret_value_bytes > self.max_snapshot_bytes
            || self.connect_timeout_ms > self.attempt_timeout_ms
            || self.attempt_timeout_ms > self.operation_timeout_ms
            || self.operation_timeout_ms > self.preparation_timeout_ms
        {
            return Err(PreparationError::InvalidLimits);
        }
        Ok(())
    }
}

fn valid_lookup_key(key: &str) -> bool {
    !key.is_empty() && !key.chars().all(char::is_whitespace) && !key.chars().any(char::is_control)
}

fn valid_variable_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_agent_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 128
        && !matches!(segment, "." | "..")
        && !segment.chars().any(|character| {
            character.is_control() || matches!(character, '/' | '\\' | '%' | '?' | '#')
        })
}

fn agent_endpoint(endpoint: &str) -> Result<Url, PreparationError> {
    // Check the original authority too: URL parsing otherwise accepts integer/octal
    // IPv4, backslashes and dot paths while changing their spelling/meaning.
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or(PreparationError::InvalidEndpoint)?;
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains(['/', '\\', '@', '?', '#', '%'])
        || endpoint
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(PreparationError::InvalidEndpoint);
    }
    let host = if let Some(ipv6) = authority.strip_prefix('[') {
        ipv6.split_once(']').map(|(host, _port)| host)
    } else {
        Some(
            authority
                .split_once(':')
                .map_or(authority, |(host, _port)| host),
        )
    }
    .ok_or(PreparationError::InvalidEndpoint)?;
    let literal = host
        .parse::<IpAddr>()
        .map_err(|_error| PreparationError::InvalidEndpoint)?;
    let url = Url::parse(endpoint).map_err(|_error| PreparationError::InvalidEndpoint)?;
    if !literal.is_loopback()
        || url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.port_or_known_default().is_none_or(|port| port == 0)
        || url.host_str().is_none()
    {
        return Err(PreparationError::InvalidEndpoint);
    }
    Ok(url)
}

fn secret_region(arn: &str) -> Result<&str, PreparationError> {
    if arn.len() > 2_048 {
        return Err(PreparationError::InvalidResource);
    }
    let parts: Vec<&str> = arn.split(':').collect();
    let [prefix, partition, service, region, account, resource, name] = parts.as_slice() else {
        return Err(PreparationError::InvalidResource);
    };
    let Some((secret_name, suffix)) = name.rsplit_once('-') else {
        return Err(PreparationError::InvalidResource);
    };
    if *prefix != "arn"
        || *service != "secretsmanager"
        || *resource != "secret"
        || account.len() != 12
        || !account.bytes().all(|byte| byte.is_ascii_digit())
        || secret_name.is_empty()
        || secret_name.len() > 512
        || !secret_name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'_' | b'+' | b'=' | b'.' | b'@' | b'-')
        })
        || suffix.len() != 6
        || !suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        || arn.len() > 2_048
        || !valid_region(partition, region)
    {
        return Err(PreparationError::InvalidResource);
    }
    Ok(region)
}

fn valid_region(partition: &str, region: &str) -> bool {
    let Some((family, number)) = region.rsplit_once('-') else {
        return false;
    };
    if number.is_empty()
        || !number.bytes().all(|byte| byte.is_ascii_digit())
        || number.starts_with('0')
        || !family
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_lowercase()))
    {
        return false;
    }
    // Supported partition families, not an assertion that an arbitrary matching
    // region is currently served. Isolated/sovereign partitions are not v1 support.
    match partition {
        "aws-cn" => family.starts_with("cn-"),
        "aws-us-gov" => family.starts_with("us-gov-"),
        "aws" => {
            [
                "af-", "ap-", "ca-", "eu-", "il-", "me-", "mx-", "sa-", "us-",
            ]
            .iter()
            .any(|prefix| family.starts_with(prefix))
                && !family.starts_with("us-gov-")
                && !family.starts_with("us-iso")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic_in_result_fn,
        reason = "test functions combine checked setup with behavioral assertions"
    )]
    #![expect(
        clippy::assertions_on_result_states,
        reason = "malformed input matrix observes parser acceptance without unwrapping"
    )]

    use std::borrow::Cow;
    use std::collections::BTreeMap;

    use percent_encoding::percent_decode_str;
    use url::Url;

    use super::{
        AgentAccessToken, AgentDocument, AppConfigAgentSettings, AwsPreparationLimits,
        SecretTarget, SecretVersion, SecretsManagerSettings,
    };
    use crate::PreparationError;

    fn agent(endpoint: &str) -> AppConfigAgentSettings {
        AppConfigAgentSettings {
            default_key: "settings".to_owned(),
            endpoint: endpoint.to_owned(),
            access_token_env: None,
            documents: BTreeMap::from([(
                "settings".to_owned(),
                AgentDocument {
                    application: "example-app".to_owned(),
                    environment: "production".to_owned(),
                    profile: "application-envelope".to_owned(),
                },
            )]),
        }
    }

    #[test]
    fn literal_loopback_and_exact_request_path() -> Result<(), PreparationError> {
        for endpoint in [
            "http://127.0.0.1:2772",
            "http://127.1.2.3:2772/",
            "http://[::1]:2772",
        ] {
            let settings = agent(endpoint);
            settings.validate()?;
            let document = settings
                .documents
                .get("settings")
                .ok_or(PreparationError::InvalidBinding)?;
            assert_eq!(
                settings.document_url(document)?.path(),
                "/applications/example-app/environments/production/configurations/application-envelope"
            );
        }
        let mut settings = agent("http://127.0.0.1:2772");
        if let Some(document) = settings.documents.get_mut("settings") {
            document.profile = "Profile name \u{fc}".to_owned();
        }
        let document = settings
            .documents
            .get("settings")
            .ok_or(PreparationError::InvalidBinding)?;
        assert_eq!(
            settings.document_url(document)?.path(),
            "/applications/example-app/environments/production/configurations/Profile%20name%20%C3%BC"
        );
        Ok(())
    }

    #[test]
    fn endpoints_and_segments_fail_without_echoing_inputs() {
        for endpoint in [
            "http://localhost:2772",
            "https://127.0.0.1",
            "http://192.0.2.1",
            "http://user:sentinel@127.0.0.1",
            "http://127.0.0.1?sentinel",
            "http://127.0.0.1#sentinel",
            "http://127.0.0.1/x/..",
            "http://127.0.0.1/%2e",
            "http://2130706433",
            "http://0177.0.0.1",
            "http://127.0.0.1\\sentinel",
            "http://[::1%25sentinel]",
            "http://127.0.0.1:0",
            "http://127.0.0.1:65536",
            " http://127.0.0.1",
            "http://127.0.0.1//",
        ] {
            let settings = agent(endpoint);
            assert!(settings.validate().is_err(), "accepted unsafe endpoint");
            assert!(!format!("{settings:?}").contains("sentinel"));
        }
        for profile in ["", ".", "..", "a/b", "a\\b", "a%2Fb", "a?b", "a#b", "a\nb"] {
            let mut settings = agent("http://127.0.0.1:2772");
            if let Some(document) = settings.documents.get_mut("settings") {
                document.profile = profile.to_owned();
            }
            assert!(settings.validate().is_err());
        }
    }

    #[test]
    fn defaults_tokens_and_mapping_keys_are_strict() {
        let mut settings = agent("http://127.0.0.1:2772");
        settings.default_key = "Settings".to_owned();
        assert_eq!(settings.validate(), Err(PreparationError::InvalidBinding));
        settings.default_key = "settings".to_owned();
        settings.access_token_env = Some("TOKEN=sentinel".to_owned());
        assert_eq!(settings.validate(), Err(PreparationError::InvalidBinding));
        assert!(!format!("{settings:?}").contains("sentinel"));
        for value in ["", "\n", "a b", "\u{fc}"] {
            assert!(AgentAccessToken::new(value.to_owned()).is_err());
        }
        let token = AgentAccessToken::new("sentinel-secret".to_owned());
        assert!(token.is_ok());
        assert!(!format!("{token:?}").contains("sentinel"));
    }

    #[test]
    fn complete_arns_regions_and_cross_account_references() -> Result<(), PreparationError> {
        for (partition, region) in [
            ("aws", "us-east-1"),
            ("aws-cn", "cn-north-1"),
            ("aws-us-gov", "us-gov-west-1"),
        ] {
            let target = SecretTarget::new(
                format!(
                    "arn:{partition}:secretsmanager:{region}:999988887777:secret:example/key-AbCdEf"
                ),
                SecretVersion::Current,
            )?;
            let mut settings = SecretsManagerSettings {
                region: region.to_owned(),
                entries: BTreeMap::from([("active".to_owned(), target)]),
            };
            settings.validate()?;
            settings.region = "us-west-2".to_owned();
            assert_eq!(settings.validate(), Err(PreparationError::InvalidResource));
        }
        for arn in [
            "name",
            "arn:aws:secretsmanager:us-east-1:111122223333:secret:sentinel",
            "arn:aws:s3:us-east-1:111122223333:secret:sentinel-AbCdEf",
            "arn:aws-cn:secretsmanager:us-east-1:111122223333:secret:sentinel-AbCdEf",
            "arn:aws:secretsmanager:us-gov-west-1:111122223333:secret:sentinel-AbCdEf",
            "arn:aws:secretsmanager:us-east-0:111122223333:secret:sentinel-AbCdEf",
            "arn:aws:secretsmanager::111122223333:secret:sentinel-AbCdEf",
            "arn:aws:secretsmanager:us-east-1:123:secret:sentinel-AbCdEf",
            "arn:aws:secretsmanager:us-east-1:111122223333:other:sentinel-AbCdEf",
            "arn:aws:secretsmanager:us-east-1:111122223333:secret:sentinel*-AbCdEf",
            "arn:aws:secretsmanager:us-east-1:111122223333:secret:sentinel-AbCdEf:extra",
        ] {
            let error = SecretTarget::new(arn.to_owned(), SecretVersion::Current).err();
            assert_eq!(error, Some(PreparationError::InvalidResource));
            assert!(!format!("{error:?}").contains("sentinel"));
        }
        Ok(())
    }

    #[test]
    fn selectors_are_exclusive_and_length_checked() {
        let arn = "arn:aws:secretsmanager:us-east-1:111122223333:secret:sentinel-AbCdEf";
        for version in [
            SecretVersion::Stage(String::new()),
            SecretVersion::Stage("x".repeat(257)),
            SecretVersion::VersionId("x".repeat(31)),
            SecretVersion::VersionId("x".repeat(65)),
            SecretVersion::Stage("sentinel\n".to_owned()),
        ] {
            assert_eq!(
                SecretTarget::new(arn.to_owned(), version).err(),
                Some(PreparationError::InvalidSelector)
            );
        }
        for selector in [
            "",
            "version_stage = 'AWSCURRENT'",
            "version_id = '12345678901234567890123456789012'",
        ] {
            assert!(
                toml::from_str::<SecretTarget>(&format!("secret_id = '{arn}'\n{selector}")).is_ok()
            );
        }
        for suffix in [
            "version_stage = 'AWSCURRENT'\nversion_id = '12345678901234567890123456789012'",
            "unknown = 'sentinel'",
        ] {
            assert!(
                toml::from_str::<SecretTarget>(&format!("secret_id = '{arn}'\n{suffix}")).is_err()
            );
        }
    }

    #[test]
    fn secret_keys_fit_portable_handle_and_current_selectors_normalize()
    -> Result<(), PreparationError> {
        let arn = "arn:aws:secretsmanager:us-east-1:111122223333:secret:example-AbCdEf";
        let current = SecretTarget::new(arn.to_owned(), SecretVersion::Current)?;
        let explicit = SecretTarget::new(
            arn.to_owned(),
            SecretVersion::Stage("AWSCURRENT".to_owned()),
        )?;
        assert_eq!(current, explicit);
        let mut settings = SecretsManagerSettings {
            region: "us-east-1".to_owned(),
            entries: BTreeMap::from([("\u{fc}".repeat(256), current)]),
        };
        settings.validate()?;
        let target = settings
            .entries
            .values()
            .next()
            .cloned()
            .ok_or(PreparationError::InvalidBinding)?;
        settings.entries = BTreeMap::from([("\u{fc}".repeat(257), target)]);
        assert_eq!(settings.validate(), Err(PreparationError::InvalidBinding));
        Ok(())
    }

    #[test]
    fn allowed_segments_round_trip_after_serialization_and_decoding() -> Result<(), PreparationError>
    {
        let mut settings = agent("http://127.0.0.1:2772");
        settings.documents.insert(
            "settings".to_owned(),
            AgentDocument {
                application: "App +@=".to_owned(),
                environment: "Env:;name".to_owned(),
                profile: "Profile \u{fc}".to_owned(),
            },
        );
        let target = settings
            .documents
            .get("settings")
            .ok_or(PreparationError::InvalidBinding)?;
        let serialized = settings.document_url(target)?.to_string();
        let reparsed =
            Url::parse(&serialized).map_err(|_error| PreparationError::InvalidEndpoint)?;
        let decoded: Vec<String> = reparsed
            .path_segments()
            .ok_or(PreparationError::InvalidEndpoint)?
            .map(|segment| {
                percent_decode_str(segment)
                    .decode_utf8()
                    .map(Cow::into_owned)
                    .map_err(|_error| PreparationError::InvalidResource)
            })
            .collect::<Result<_, _>>()?;
        assert_eq!(
            decoded,
            [
                "applications",
                "App +@=",
                "environments",
                "Env:;name",
                "configurations",
                "Profile \u{fc}"
            ]
        );
        Ok(())
    }

    #[test]
    fn selector_boundaries_preserve_unicode_and_embedded_spaces() -> Result<(), PreparationError> {
        let arn = "arn:aws:secretsmanager:us-east-1:111122223333:secret:example-AbCdEf";
        for selector in [
            SecretVersion::Stage("x".to_owned()),
            SecretVersion::Stage("\u{fc}".repeat(256)),
            SecretVersion::Stage("a b".to_owned()),
            SecretVersion::VersionId("\u{fc}".repeat(32)),
            SecretVersion::VersionId("x".repeat(64)),
        ] {
            let target = SecretTarget::new(arn.to_owned(), selector.clone())?;
            assert_eq!(target.version(), &selector);
        }
        assert_eq!(
            SecretTarget::new(arn.to_owned(), SecretVersion::Stage(" ".repeat(256))).err(),
            Some(PreparationError::InvalidSelector)
        );
        Ok(())
    }

    #[test]
    fn arn_size_name_and_region_shape_boundaries() -> Result<(), PreparationError> {
        let valid = format!(
            "arn:aws:secretsmanager:us-east-1:111122223333:secret:{}-AbCdEf",
            "x".repeat(512)
        );
        SecretTarget::new(valid, SecretVersion::Current)?;
        let oversized_name = format!(
            "arn:aws:secretsmanager:us-east-1:111122223333:secret:{}-AbCdEf",
            "x".repeat(513)
        );
        assert_eq!(
            SecretTarget::new(oversized_name, SecretVersion::Current).err(),
            Some(PreparationError::InvalidResource)
        );
        // Family shape does not promise an existing/served region.
        SecretTarget::new(
            "arn:aws:secretsmanager:us-fictional-99:111122223333:secret:x-AbCdEf".to_owned(),
            SecretVersion::Current,
        )?;
        let base = "arn:aws:secretsmanager::111122223333:secret:x-AbCdEf";
        let maximum_region = format!("us-{}-1", "a".repeat(2_048 - base.len() - 5));
        let maximum =
            format!("arn:aws:secretsmanager:{maximum_region}:111122223333:secret:x-AbCdEf");
        assert_eq!(maximum.len(), 2_048);
        SecretTarget::new(maximum.clone(), SecretVersion::Current)?;
        assert_eq!(
            SecretTarget::new(format!("{maximum}x"), SecretVersion::Current).err(),
            Some(PreparationError::InvalidResource)
        );
        for region in [
            "zz-east-1",
            "us--east-1",
            "us-East-1",
            "us-east-01",
            "us-east-x",
        ] {
            assert_eq!(
                SecretTarget::new(
                    format!("arn:aws:secretsmanager:{region}:111122223333:secret:x-AbCdEf"),
                    SecretVersion::Current
                )
                .err(),
                Some(PreparationError::InvalidResource)
            );
        }
        Ok(())
    }

    #[test]
    fn configurable_limits_defaults_consistency_and_unknown_fields() -> Result<(), PreparationError>
    {
        let defaults = AwsPreparationLimits::default();
        defaults.validate()?;
        assert_eq!(defaults.max_snapshot_bytes, 8 * 1_024 * 1_024);
        let raised: AwsPreparationLimits = toml::from_str("max_snapshot_bytes = 16777216")
            .map_err(|_error| PreparationError::InvalidLimits)?;
        raised.validate()?;
        assert_eq!(
            raised.max_config_document_bytes,
            defaults.max_config_document_bytes
        );
        for input in [
            "max_snapshot_bytes = 0",
            "max_snapshot_bytes = 1",
            "max_remote_values = -1",
            "operation_timeout_ms = 0",
            "connect_timeout_ms = 2001",
            "attempt_timeout_ms = 5001",
            "operation_timeout_ms = 30001",
            "unknown = 1",
            "max_remote_values = 18446744073709551616",
        ] {
            let parsed = toml::from_str::<AwsPreparationLimits>(input);
            assert!(parsed.is_err() || parsed.is_ok_and(|limits| limits.validate().is_err()));
        }
        Ok(())
    }
}
