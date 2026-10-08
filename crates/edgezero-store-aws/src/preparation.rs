//! Native service preparation session and immutable `AppConfig` snapshots.
//!
//! This module is enabled only when at least one native AWS store service is
//! selected. The session's counters and caches are private to one startup.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
#[cfg(feature = "appconfig-agent")]
use std::sync::Arc;
use std::time::Duration;

use edgezero_core::config_store::BoundedStoreRead;
#[cfg(feature = "appconfig-agent")]
use edgezero_core::config_store::ConfigStoreHandle;
#[cfg(feature = "appconfig-agent")]
use edgezero_core::config_store::{
    ConfigStore, ConfigStoreError, ConfigValue, finish_bounded_config_read,
};
#[cfg(test)]
#[cfg(feature = "appconfig-agent")]
use edgezero_core::config_store_contract_tests;
#[cfg(feature = "secrets-manager")]
use edgezero_core::secret_store::{SecretError, SecretStore, finish_bounded_secret_read};
use edgezero_core::time::{Deadline, MonotonicClock};
#[cfg(feature = "appconfig-agent")]
use futures_util::StreamExt as _;
#[cfg(feature = "appconfig-agent")]
use reqwest::header::{ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING, HeaderValue};
#[cfg(feature = "appconfig-agent")]
use reqwest::{Client, ClientBuilder, redirect::Policy};
#[cfg(feature = "appconfig-agent")]
use tokio::time::timeout;

use crate::PreparationError;
use crate::settings::AwsPreparationLimits;
#[cfg(feature = "appconfig-agent")]
use crate::settings::{AgentAccessToken, AppConfigAgentSettings};

/// Aggregate startup preparation. Never share this object across startup runs.
#[expect(
    clippy::field_scoped_visibility_modifiers,
    reason = "the session shares selected state with its feature-gated Secrets Manager sibling module"
)]
pub struct AwsPreparation {
    accounting: Accounting,
    #[cfg(feature = "appconfig-agent")]
    appconfig_cache: HashMap<AppConfigTargetKey, ConfigValue>,
    #[cfg(feature = "appconfig-agent")]
    appconfig_client: Option<Client>,
    pub(crate) clock: MonotonicClock,
    pub(crate) deadline: Deadline,
    pub(crate) limits: AwsPreparationLimits,
    #[cfg(feature = "secrets-manager")]
    pub(crate) secret_cache: HashMap<SecretTargetKey, bytes::Bytes>,
    #[cfg(feature = "secrets-manager")]
    pub(crate) secret_clients: HashMap<SecretClientKey, SecretClient>,
}

impl fmt::Debug for AwsPreparation {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsPreparation")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "keep startup entry points and feature-gated service operations grouped ahead of private helpers"
)]
impl AwsPreparation {
    /// Start the aggregate deadline before creating any service client.
    ///
    /// A timeout bounds observation of async work, not arbitrary blocking provider
    /// work or subprocesses: dropping the future is not a hard wall-clock guarantee.
    ///
    /// # Errors
    ///
    /// Returns [`PreparationError::InvalidLimits`] when limits are invalid or the
    /// aggregate deadline cannot be represented.
    #[inline]
    pub fn new(
        clock: MonotonicClock,
        limits: AwsPreparationLimits,
    ) -> Result<Self, PreparationError> {
        limits.validate()?;
        let duration = Duration::from_millis(limits.preparation_timeout_ms);
        let deadline = clock
            .now()
            .checked_add(duration)
            .map(Deadline::at_instant)
            .ok_or(PreparationError::InvalidLimits)?;
        Ok(Self {
            clock,
            deadline,
            limits,
            accounting: Accounting::default(),
            #[cfg(feature = "appconfig-agent")]
            appconfig_client: None,
            #[cfg(feature = "appconfig-agent")]
            appconfig_cache: HashMap::new(),
            #[cfg(feature = "secrets-manager")]
            secret_cache: HashMap::new(),
            #[cfg(feature = "secrets-manager")]
            secret_clients: HashMap::new(),
        })
    }

    /// Fetch and freeze all mapped `AppConfig` Agent documents into a core handle.
    ///
    /// # Errors
    ///
    /// Returns a sanitized [`PreparationError`] when settings, authorization, the
    /// deadline, a response, or a configured size limit prevents preparation.
    #[inline]
    #[cfg(feature = "appconfig-agent")]
    pub async fn prepare_appconfig(
        &mut self,
        settings: &AppConfigAgentSettings,
        token: Option<&AgentAccessToken>,
    ) -> Result<ConfigStoreHandle, PreparationError> {
        settings.validate()?;
        if settings.access_token_env.is_some() != token.is_some() {
            return Err(PreparationError::InvalidBinding);
        }
        let mut targets = Vec::new();
        for (key, document) in &settings.documents {
            self.check_deadline()?;
            let url = settings.document_url(document)?;
            let auth = token.map_or_else(
                || AuthorizationIdentity::NoBearer {
                    env_name: settings.access_token_env.clone(),
                },
                |value| AuthorizationIdentity::Bearer {
                    env_name: settings.access_token_env.clone(),
                    token: value.expose().to_owned(),
                },
            );
            let target = AppConfigTargetKey {
                url: url.as_str().to_owned(),
                auth,
            };
            let auth_metadata = match &target.auth {
                AuthorizationIdentity::NoBearer { env_name } => {
                    env_name.as_ref().map_or(0, String::len)
                }
                AuthorizationIdentity::Bearer {
                    env_name,
                    token: bearer_token,
                } => env_name
                    .as_ref()
                    .map_or(Some(0), |name| Some(name.len()))
                    .and_then(|bytes| bytes.checked_add(bearer_token.len()))
                    .ok_or(PreparationError::SizeLimit)?,
            };
            let target_metadata = target
                .url
                .len()
                .checked_add(auth_metadata)
                .ok_or(PreparationError::SizeLimit)?;
            self.accounting
                .charge_mapping(&self.limits, key.len(), 0, target_metadata)?;
            targets.push((key, url, target));
        }
        let mut entries = BTreeMap::new();
        for (key, url, target) in targets {
            self.check_deadline()?;
            let value = if let Some(value) = self.appconfig_cache.get(&target) {
                value.clone()
            } else {
                let fetched = self.fetch_agent_document(url, token).await?;
                self.accounting
                    .charge_retained(&self.limits, fetched.as_ref().len())?;
                self.appconfig_cache.insert(target, fetched.clone());
                fetched
            };
            entries.insert(key.clone(), value);
        }
        self.check_deadline()?;
        Ok(ConfigStoreHandle::new(Arc::new(ConfigSnapshot { entries })))
    }

    /// Safe counters only; no identifiers, lengths of individual values, or auth data.
    #[must_use]
    #[inline]
    pub fn stats(&self) -> PreparationStats {
        PreparationStats {
            mappings: self.accounting.mapping_count,
            unique_retained_payload_bytes: self.accounting.unique_retained_payload_bytes,
            metadata_bytes: self.accounting.metadata_bytes,
            peak_transient_staging_bytes: self.accounting.peak_transient_staging_bytes,
        }
    }

    pub(crate) fn check_deadline(&self) -> Result<Duration, PreparationError> {
        self.deadline
            .remaining_at(self.clock.now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(PreparationError::Deadline)
    }

    /// Current operation allowance, clipped to the shared deadline.
    pub(crate) fn operation_budget(&self) -> Result<Duration, PreparationError> {
        Ok(self
            .check_deadline()?
            .min(Duration::from_millis(self.limits.operation_timeout_ms)))
    }

    /// Charge one logical mapping before cache lookup/deduplication.
    #[cfg(feature = "secrets-manager")]
    pub(crate) fn charge_mapping(
        &mut self,
        key_bytes: usize,
        namespace_bytes: usize,
        target_metadata_bytes: usize,
    ) -> Result<(), PreparationError> {
        self.accounting.charge_mapping(
            &self.limits,
            key_bytes,
            namespace_bytes,
            target_metadata_bytes,
        )
    }

    /// Retain one successful unique payload. Call only after all value caps pass.
    #[cfg(feature = "secrets-manager")]
    pub(crate) fn charge_retained(&mut self, bytes: usize) -> Result<(), PreparationError> {
        self.accounting.charge_retained(&self.limits, bytes)
    }

    #[cfg(feature = "appconfig-agent")]
    async fn fetch_agent_document(
        &mut self,
        url: url::Url,
        token: Option<&AgentAccessToken>,
    ) -> Result<ConfigValue, PreparationError> {
        self.check_deadline()?;
        let client = if let Some(client) = &self.appconfig_client {
            client.clone()
        } else {
            let client = ClientBuilder::new()
                .no_proxy()
                .connection_verbose(false)
                .redirect(Policy::none())
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .connect_timeout(
                    self.operation_budget()?
                        .min(Duration::from_millis(self.limits.connect_timeout_ms)),
                )
                .build()
                .map_err(|_error| PreparationError::Unavailable)?;
            self.check_deadline()?;
            self.appconfig_client = Some(client.clone());
            client
        };
        let budget = self
            .operation_budget()?
            .min(Duration::from_millis(self.limits.attempt_timeout_ms));
        let mut request = client
            .get(url)
            .header(ACCEPT_ENCODING, "identity")
            .timeout(budget);
        if let Some(access_token) = token {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", access_token.expose()))
                .map_err(|_error| PreparationError::InvalidBinding)?;
            value.set_sensitive(true);
            request = request.header(AUTHORIZATION, value);
        }
        let remaining_payload = self
            .limits
            .max_snapshot_bytes
            .checked_sub(self.accounting.unique_retained_payload_bytes)
            .ok_or(PreparationError::SizeLimit)?;
        let cap = usize::try_from(self.limits.max_config_document_bytes.min(remaining_payload))
            .map_err(|_error| PreparationError::InvalidLimits)?;
        let fetched = timeout(budget, async {
            let response = request.send().await.map_err(map_http_error)?;
            if !response.status().is_success() {
                return Err(match response.status().as_u16() {
                    401 | 403 => PreparationError::AccessDenied,
                    404 => PreparationError::Missing,
                    429 => PreparationError::Throttled,
                    _ => PreparationError::Unavailable,
                });
            }
            if response.headers().get_all(CONTENT_ENCODING).iter().count() > 1
                || response
                    .headers()
                    .get_all(CONTENT_ENCODING)
                    .iter()
                    .any(|value| {
                        value.to_str().map_or(true, |encoding| {
                            !encoding.trim().eq_ignore_ascii_case("identity")
                        })
                    })
            {
                return Err(PreparationError::Malformed);
            }
            if response
                .content_length()
                .is_some_and(|length| length > u64::try_from(cap).unwrap_or(u64::MAX))
            {
                return Err(PreparationError::SizeLimit);
            }
            let mut stream = response.bytes_stream();
            let mut body = Vec::with_capacity(cap.min(16 * 1024));
            while let Some(chunk_result) = stream.next().await {
                let chunk_bytes = chunk_result.map_err(map_http_error)?;
                if chunk_bytes.len() > cap.saturating_sub(body.len()) {
                    return Err(PreparationError::SizeLimit);
                }
                self.check_deadline()?;
                self.accounting.observe_staging(
                    body.len()
                        .checked_add(chunk_bytes.len())
                        .ok_or(PreparationError::SizeLimit)?,
                )?;
                body.extend_from_slice(&chunk_bytes);
            }
            String::from_utf8(body).map_err(|_error| PreparationError::Malformed)
        })
        .await
        .map_err(|_elapsed| PreparationError::Deadline)?;
        self.check_deadline()?;
        let text = fetched?;
        // Preserve every UTF-8 byte, including empty/non-JSON free-form documents.
        self.accounting.observe_staging(text.len())?;
        let value = ConfigValue::from(text);
        self.check_deadline()?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreparationStats {
    pub mappings: u64,
    pub metadata_bytes: u64,
    pub peak_transient_staging_bytes: u64,
    pub unique_retained_payload_bytes: u64,
}

#[derive(Default)]
struct Accounting {
    mapping_count: u64,
    metadata_bytes: u64,
    peak_transient_staging_bytes: u64,
    unique_retained_payload_bytes: u64,
}

impl Accounting {
    fn charge_mapping(
        &mut self,
        limits: &AwsPreparationLimits,
        key_bytes: usize,
        namespace_bytes: usize,
        target_metadata_bytes: usize,
    ) -> Result<(), PreparationError> {
        let mapping_count = self
            .mapping_count
            .checked_add(1)
            .filter(|count| *count <= limits.max_remote_values)
            .ok_or(PreparationError::SizeLimit)?;
        let metadata = key_bytes
            .checked_add(namespace_bytes)
            .and_then(|value| value.checked_add(target_metadata_bytes))
            .and_then(|value| u64::try_from(value).ok())
            .and_then(|value| self.metadata_bytes.checked_add(value))
            .ok_or(PreparationError::SizeLimit)?;
        // Metadata has no independent configured ceiling. Checked arithmetic keeps
        // the diagnostic statistic representable without pretending it is an RSS cap.
        self.mapping_count = mapping_count;
        self.metadata_bytes = metadata;
        Ok(())
    }

    fn charge_retained(
        &mut self,
        limits: &AwsPreparationLimits,
        bytes: usize,
    ) -> Result<(), PreparationError> {
        let byte_count = u64::try_from(bytes).map_err(|_error| PreparationError::SizeLimit)?;
        let total = self
            .unique_retained_payload_bytes
            .checked_add(byte_count)
            .filter(|total| *total <= limits.max_snapshot_bytes)
            .ok_or(PreparationError::SizeLimit)?;
        self.unique_retained_payload_bytes = total;
        Ok(())
    }

    #[cfg(feature = "appconfig-agent")]
    fn observe_staging(&mut self, bytes: usize) -> Result<(), PreparationError> {
        let byte_count = u64::try_from(bytes).map_err(|_error| PreparationError::SizeLimit)?;
        self.peak_transient_staging_bytes = self.peak_transient_staging_bytes.max(byte_count);
        Ok(())
    }
}

#[cfg(feature = "appconfig-agent")]
#[derive(Clone, Eq, Hash, PartialEq)]
struct AppConfigTargetKey {
    auth: AuthorizationIdentity,
    url: String,
}

#[cfg(feature = "appconfig-agent")]
#[derive(Clone, Eq, Hash, PartialEq)]
enum AuthorizationIdentity {
    Bearer {
        env_name: Option<String>,
        token: String,
    },
    NoBearer {
        env_name: Option<String>,
    },
}

#[cfg(feature = "appconfig-agent")]
impl fmt::Debug for AuthorizationIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthorizationIdentity([redacted])")
    }
}

#[cfg(feature = "appconfig-agent")]
impl fmt::Debug for AppConfigTargetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppConfigTargetKey([redacted])")
    }
}

#[cfg(feature = "secrets-manager")]
#[derive(Clone, Eq, Hash, PartialEq)]
#[expect(
    clippy::field_scoped_visibility_modifiers,
    reason = "the client cache key is shared only with the feature-gated Secrets Manager sibling module"
)]
pub(crate) struct SecretClientKey {
    pub(crate) policy: String,
    pub(crate) region: String,
}

#[cfg(feature = "secrets-manager")]
#[expect(
    clippy::field_scoped_visibility_modifiers,
    reason = "the SDK client is shared only with the feature-gated Secrets Manager sibling module"
)]
pub(crate) struct SecretClient {
    pub(crate) client: aws_sdk_secretsmanager::Client,
}

#[cfg(feature = "secrets-manager")]
#[derive(Clone, Eq, Hash, PartialEq)]
#[expect(
    clippy::field_scoped_visibility_modifiers,
    reason = "the target cache key is shared only with the feature-gated Secrets Manager sibling module"
)]
pub(crate) struct SecretTargetKey {
    pub(crate) client_policy: String,
    pub(crate) region: String,
    pub(crate) secret_id: String,
    /// Exact private selector identity; never include in diagnostics.
    pub(crate) selector: String,
}

#[cfg(feature = "secrets-manager")]
impl fmt::Debug for SecretTargetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretTargetKey([redacted])")
    }
}

#[cfg(feature = "appconfig-agent")]
struct ConfigSnapshot {
    entries: BTreeMap<String, ConfigValue>,
}

#[cfg(feature = "appconfig-agent")]
impl fmt::Debug for ConfigSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigSnapshot")
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

#[cfg(feature = "appconfig-agent")]
#[async_trait::async_trait(?Send)]
impl ConfigStore for ConfigSnapshot {
    async fn get(&self, key: &str) -> Result<Option<ConfigValue>, ConfigStoreError> {
        Ok(self.entries.get(key).cloned())
    }

    async fn get_bounded(
        &self,
        key: &str,
        clock: &MonotonicClock,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<ConfigValue>, ConfigStoreError> {
        if deadline.is_expired_at(clock.now()) {
            return Err(ConfigStoreError::DeadlineExceeded);
        }
        let result = finish_bounded_config_read(
            Ok(self.entries.get(key).cloned()),
            clock,
            deadline,
            max_backend_bytes,
            max_value_bytes,
        );
        if deadline.is_expired_at(clock.now()) {
            return Err(ConfigStoreError::DeadlineExceeded);
        }
        result
    }
}

#[cfg(feature = "secrets-manager")]
#[expect(
    clippy::field_scoped_visibility_modifiers,
    reason = "the immutable snapshot is constructed by the feature-gated Secrets Manager sibling module"
)]
pub(crate) struct SecretSnapshot {
    pub(crate) entries: BTreeMap<String, bytes::Bytes>,
    pub(crate) namespace: String,
}

#[cfg(feature = "secrets-manager")]
impl fmt::Debug for SecretSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretSnapshot")
            .field("entry_count", &self.entries.len())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "secrets-manager")]
#[async_trait::async_trait(?Send)]
impl SecretStore for SecretSnapshot {
    async fn get_bytes(
        &self,
        store_name: &str,
        key: &str,
    ) -> Result<Option<bytes::Bytes>, SecretError> {
        Ok((store_name == self.namespace)
            .then(|| self.entries.get(key).cloned())
            .flatten())
    }

    async fn get_bytes_bounded(
        &self,
        store_name: &str,
        key: &str,
        clock: &MonotonicClock,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<bytes::Bytes>, SecretError> {
        if deadline.is_expired_at(clock.now()) {
            return Err(SecretError::DeadlineExceeded);
        }
        let value = (store_name == self.namespace)
            .then(|| self.entries.get(key).cloned())
            .flatten();
        let result = finish_bounded_secret_read(
            Ok(value),
            clock,
            deadline,
            max_backend_bytes,
            max_value_bytes,
        );
        if deadline.is_expired_at(clock.now()) {
            return Err(SecretError::DeadlineExceeded);
        }
        result
    }
}

#[cfg(feature = "appconfig-agent")]
fn map_http_error(_error: reqwest::Error) -> PreparationError {
    PreparationError::Unavailable
}

#[cfg(test)]
#[cfg(feature = "appconfig-agent")]
mod tests {
    use super::*;
    use edgezero_core::time::MonotonicInstant;
    use futures::executor::block_on;
    use std::sync::Mutex;

    fn agent_settings(endpoint: &str) -> AppConfigAgentSettings {
        use crate::settings::AgentDocument;
        AppConfigAgentSettings {
            default_key: "main".to_owned(),
            endpoint: endpoint.to_owned(),
            access_token_env: Some("EDGE_TOKEN".to_owned()),
            documents: BTreeMap::from([(
                "main".to_owned(),
                AgentDocument {
                    application: "app".to_owned(),
                    environment: "test".to_owned(),
                    profile: "profile".to_owned(),
                },
            )]),
        }
    }

    fn test_clock() -> MonotonicClock {
        MonotonicClock::default()
    }

    #[test]
    fn absolute_deadline_is_checked_and_snapshot_contract_is_ordinary() {
        let clock = test_clock();
        let _session = AwsPreparation::new(clock, AwsPreparationLimits::default())
            .expect("default limits produce a valid preparation session");
        let accounting = Accounting::default();
        assert_eq!(accounting.mapping_count, 0);
    }

    #[test]
    fn accounting_charges_aliases_before_dedup_and_checks_overflow() {
        let limits = AwsPreparationLimits {
            max_remote_values: 1,
            ..AwsPreparationLimits::default()
        };
        let mut accounting = Accounting::default();
        accounting
            .charge_mapping(&limits, 3, 0, 4)
            .expect("first mapping is within the configured limit");
        assert_eq!(
            accounting.charge_mapping(&limits, 3, 0, 4),
            Err(PreparationError::SizeLimit)
        );
        accounting.unique_retained_payload_bytes = u64::MAX;
        assert_eq!(
            accounting.charge_retained(&limits, 1),
            Err(PreparationError::SizeLimit)
        );
    }

    #[tokio::test]
    async fn appconfig_real_loopback_fetch_and_exact_body_snapshot() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request_bytes = vec![0_u8; 4096];
            let count = socket.read(&mut request_bytes).await.expect("read request");
            let request_text = String::from_utf8_lossy(&request_bytes[..count]);
            assert!(
                request_text
                    .to_ascii_lowercase()
                    .contains("accept-encoding: identity")
            );
            assert!(
                request_text.contains("/applications/app/environments/test/configurations/profile")
            );
            let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"x\":\"\u{fc}\"}  ";
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        let mut settings = agent_settings(&format!("http://{address}"));
        settings.access_token_env = None;
        let mut session =
            AwsPreparation::new(test_clock(), AwsPreparationLimits::default()).expect("session");
        let handle = session
            .prepare_appconfig(&settings, None)
            .await
            .expect("prepare");
        let value = handle
            .get("main")
            .await
            .expect("read snapshot")
            .expect("entry");
        assert_eq!(value.as_ref(), "{\"x\":\"\u{fc}\"}  ");
        server.await.expect("server");
    }

    #[test]
    fn token_debug_is_redacted_even_when_target_identity_is_debugged() {
        let token = AgentAccessToken::new("very-secret-token".to_owned()).expect("token");
        assert!(!format!("{token:?}").contains("very-secret-token"));
        let key = AppConfigTargetKey {
            url: "http://127.0.0.1".to_owned(),
            auth: AuthorizationIdentity::Bearer {
                env_name: Some("EDGE_TOKEN".to_owned()),
                token: token.expose().to_owned(),
            },
        };
        assert!(!format!("{key:?}").contains("very-secret-token"));
    }

    #[test]
    fn bounded_snapshot_deadline_wins_over_size_error_at_final_check() {
        let start = MonotonicInstant::now();
        let now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&now);
        let clock = MonotonicClock::new(move || {
            let mut instant = clock_now.lock().expect("clock mutex");
            let sampled = *instant;
            *instant = instant
                .checked_add(Duration::from_millis(1))
                .expect("test instant advances");
            sampled
        });
        let deadline = Deadline::at_instant(
            start
                .checked_add(Duration::from_millis(2))
                .expect("deadline"),
        );
        let snapshot = ConfigSnapshot {
            entries: BTreeMap::from([("large".to_owned(), ConfigValue::from("payload"))]),
        };
        let result = block_on(snapshot.get_bounded("large", &clock, deadline, 1, 1));
        assert!(matches!(result, Err(ConfigStoreError::DeadlineExceeded)));
    }

    config_store_contract_tests!(appconfig_snapshot_contract, {
        ConfigSnapshot {
            entries: BTreeMap::from([
                ("contract.key.a".to_owned(), ConfigValue::from("value_a")),
                ("contract.key.b".to_owned(), ConfigValue::from("value_b")),
            ]),
        }
    });
}
