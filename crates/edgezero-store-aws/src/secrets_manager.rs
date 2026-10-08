//! Pinned AWS SDK transport and immutable Secrets Manager snapshots.
//!
//! This module is intentionally private to the native provider crate. Public
//! callers receive only a core `SecretHandle`; neither the SDK client nor the
//! remote resource map survives preparation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_sdk_secretsmanager::Client;
use aws_sdk_secretsmanager::config::timeout::TimeoutConfig;
use aws_sdk_secretsmanager::config::{Region, retry::RetryConfig};
use aws_sdk_secretsmanager::error::SdkError;
#[cfg(test)]
use aws_sdk_secretsmanager::primitives::Blob;
use bytes::Bytes;
use edgezero_core::secret_store::SecretHandle;
use tokio::time::timeout;

use crate::PreparationError;
use crate::preparation::{
    AwsPreparation, SecretClient, SecretClientKey, SecretSnapshot, SecretTargetKey,
};
use crate::settings::{SecretTarget, SecretVersion, SecretsManagerSettings};

const MAX_NAMESPACE_BYTES: usize = 512;
const DEFAULT_CLIENT_POLICY: &str = "fixed-default";

#[expect(
    clippy::multiple_inherent_impl,
    reason = "keep the feature-gated Secrets Manager transport in its provider-specific module"
)]
impl AwsPreparation {
    /// Load every explicitly mapped secret and return an immutable namespace-bound snapshot.
    ///
    /// The aggregate budget begins in `AwsPreparation::new`, before credential or SDK
    /// initialization. This method adds no outer retry loop; the pinned SDK owns at most
    /// three total attempts for each `GetSecretValue` operation.
    ///
    /// # Errors
    ///
    /// Returns a sanitized [`PreparationError`] if a binding is invalid, the
    /// aggregate deadline expires, the provider fails, or a configured size limit is exceeded.
    #[inline]
    #[expect(
        clippy::too_many_lines,
        reason = "the validate-fetch-check-charge-publish sequence is easier to audit as one operation"
    )]
    pub async fn prepare_secrets(
        &mut self,
        settings: &SecretsManagerSettings,
        namespace: &str,
    ) -> Result<SecretHandle, PreparationError> {
        // Validate all portable handle names before constructing a client or doing IO.
        if !valid_secret_name(namespace) || namespace.len() > MAX_NAMESPACE_BYTES {
            return Err(PreparationError::InvalidBinding);
        }
        settings.validate()?;
        if settings
            .entries
            .keys()
            .any(|key| !valid_secret_name(key) || key.len() > MAX_NAMESPACE_BYTES)
        {
            return Err(PreparationError::InvalidBinding);
        }

        self.check_deadline()?;
        for (key, target) in &settings.entries {
            self.charge_mapping(key.len(), namespace.len(), target_metadata_bytes(target)?)?;
        }
        let region = settings.region.clone();
        let client_key = SecretClientKey {
            region: region.clone(),
            policy: DEFAULT_CLIENT_POLICY.to_owned(),
        };
        if !self.secret_clients.contains_key(&client_key) {
            let remaining = self.operation_budget()?;
            let connect = remaining.min(Duration::from_millis(self.limits.connect_timeout_ms));
            let attempt = remaining.min(Duration::from_millis(self.limits.attempt_timeout_ms));
            let client = timeout(remaining, async {
                let shared = aws_config::defaults(BehaviorVersion::v2026_01_12())
                    .region(Region::new(region.clone()))
                    .load()
                    .await;
                self.check_deadline()?;
                let config = Client::from_conf(
                    aws_sdk_secretsmanager::Config::from(&shared)
                        .to_builder()
                        .region(Region::new(region.clone()))
                        .retry_config(RetryConfig::standard().with_max_attempts(3))
                        .timeout_config(
                            TimeoutConfig::builder()
                                .connect_timeout(connect)
                                .operation_timeout(remaining)
                                .operation_attempt_timeout(attempt)
                                .build(),
                        )
                        .build(),
                );
                self.check_deadline()?;
                Ok::<Client, PreparationError>(config)
            })
            .await
            .map_err(|_elapsed| PreparationError::Deadline)??;
            self.check_deadline()?;
            self.secret_clients
                .insert(client_key.clone(), SecretClient { client });
        }

        let client = self
            .secret_clients
            .get(&client_key)
            .map(|entry| entry.client.clone())
            .ok_or(PreparationError::Unavailable)?;
        let mut entries = BTreeMap::new();
        for (key, target) in &settings.entries {
            self.check_deadline()?;
            // Charge every logical mapping before target-cache lookup: aliases do not
            // bypass the session's configured mapping bound.
            let target_key = SecretTargetKey {
                region: region.clone(),
                client_policy: DEFAULT_CLIENT_POLICY.to_owned(),
                secret_id: target.secret_id().to_owned(),
                selector: target_selector_identity(target),
            };
            let value = if let Some(value) = self.secret_cache.get(&target_key) {
                value.clone()
            } else {
                let operation_budget = self.operation_budget()?;
                let attempt_budget =
                    operation_budget.min(Duration::from_millis(self.limits.attempt_timeout_ms));
                let connect_budget =
                    operation_budget.min(Duration::from_millis(self.limits.connect_timeout_ms));
                let mut request = client.get_secret_value().secret_id(target.secret_id());
                match target.version() {
                    SecretVersion::Current => {
                        // Omission has the service-defined AWSCURRENT meaning.
                    }
                    SecretVersion::Stage(stage) => request = request.version_stage(stage),
                    SecretVersion::VersionId(version) => request = request.version_id(version),
                }
                let per_operation = TimeoutConfig::builder()
                    .connect_timeout(connect_budget)
                    .operation_timeout(operation_budget)
                    .operation_attempt_timeout(attempt_budget)
                    .build();
                let result = timeout(
                    operation_budget,
                    request
                        .customize()
                        .config_override(
                            aws_sdk_secretsmanager::Config::builder().timeout_config(per_operation),
                        )
                        .send(),
                )
                .await
                .map_err(|_elapsed| PreparationError::Deadline)?;
                self.check_deadline()?;
                let output = result.map_err(|error| {
                    if matches!(&error, SdkError::TimeoutError(_)) {
                        return PreparationError::Deadline;
                    }
                    let service = error.as_service_error().map(|service| {
                        (
                            service.is_decryption_failure(),
                            service.is_resource_not_found_exception(),
                            service.is_invalid_request_exception(),
                            service.is_invalid_parameter_exception(),
                            service.meta().code().unwrap_or_default().to_owned(),
                        )
                    });
                    let status = error
                        .raw_response()
                        .map(|response| response.status().as_u16());
                    classify_sdk_error(service, status)
                })?;
                let cap = usize::try_from(self.limits.max_secret_value_bytes)
                    .map_err(|_error| PreparationError::InvalidLimits)?;
                let size = match (output.secret_string(), output.secret_binary()) {
                    (Some(value), None) => value.len(),
                    (None, Some(value)) => value.as_ref().len(),
                    (None, None) | (Some(_), Some(_)) => return Err(PreparationError::Malformed),
                };
                self.check_deadline()?;
                if size > cap {
                    return Err(PreparationError::SizeLimit);
                }
                // SDK response parsing/materialization happens before this check. This
                // is a retained-value cap, not an SDK RSS or transfer-allocation bound.
                self.check_deadline()?;
                self.charge_retained(size)?;
                let value = match (output.secret_string, output.secret_binary) {
                    (Some(value), None) => Bytes::from(value.into_bytes()),
                    (None, Some(value)) => Bytes::from(value.into_inner()),
                    (None, None) | (Some(_), Some(_)) => return Err(PreparationError::Malformed),
                };
                self.check_deadline()?;
                self.secret_cache.insert(target_key, value.clone());
                value
            };
            entries.insert(key.clone(), value);
        }

        self.check_deadline()?;
        Ok(SecretHandle::new(Arc::new(SecretSnapshot {
            namespace: namespace.to_owned(),
            entries,
        })))
    }
}

fn valid_secret_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_NAMESPACE_BYTES && !value.chars().any(char::is_control)
}

fn target_selector_identity(target: &SecretTarget) -> String {
    match target.version() {
        SecretVersion::Current => "stage:AWSCURRENT".to_owned(),
        SecretVersion::Stage(value) => format!("stage:{value}"),
        SecretVersion::VersionId(value) => format!("version:{value}"),
    }
}

fn target_metadata_bytes(target: &SecretTarget) -> Result<usize, PreparationError> {
    let selector_bytes = match target.version() {
        SecretVersion::Current => "AWSCURRENT".len(),
        SecretVersion::Stage(value) | SecretVersion::VersionId(value) => value.len(),
    };
    target
        .secret_id()
        .len()
        .checked_add(selector_bytes)
        .ok_or(PreparationError::SizeLimit)
}

#[cfg(test)]
fn materialize_payload(
    secret_string: Option<&str>,
    secret_binary: Option<&Blob>,
) -> Result<Bytes, PreparationError> {
    match (secret_string, secret_binary) {
        (Some(value), None) => Ok(Bytes::copy_from_slice(value.as_bytes())),
        (None, Some(value)) => Ok(Bytes::copy_from_slice(value.as_ref())),
        // Empty strings/blobs are Some(empty) and remain valid. Neither and both
        // fields are malformed service responses; never choose a precedence.
        (None, None) | (Some(_), Some(_)) => Err(PreparationError::Malformed),
    }
}

fn classify_sdk_error(
    service: Option<(bool, bool, bool, bool, String)>,
    status: Option<u16>,
) -> PreparationError {
    if let Some((decryption, not_found, invalid_request, invalid_parameter, code)) = service {
        if decryption {
            return PreparationError::Decryption;
        }
        if not_found || invalid_request {
            return PreparationError::Missing;
        }
        if invalid_parameter {
            return PreparationError::InvalidResource;
        }
        match code.as_str() {
            "AccessDeniedException" | "AccessDenied" => {
                return PreparationError::AccessDenied;
            }
            "ExpiredTokenException" | "InvalidClientTokenId" | "UnrecognizedClientException" => {
                return PreparationError::Credentials;
            }
            "ThrottlingException" | "Throttling" | "TooManyRequestsException" => {
                return PreparationError::Throttled;
            }
            _ => {}
        }
    }
    if status == Some(403) {
        return PreparationError::AccessDenied;
    }
    if status == Some(429) {
        return PreparationError::Throttled;
    }
    // SDK error/source chains are deliberately consumed and discarded here.
    // Credential-process failures are not consistently exposed as a stable
    // typed operation error by this SDK version; without an unredacted chain
    // they conservatively collapse to the closed unavailable category.
    PreparationError::Unavailable
}

#[cfg(test)]
#[expect(
    clippy::absolute_paths,
    reason = "explicit std and crate paths keep the isolated child-process fixture auditable"
)]
mod tests {
    use super::materialize_payload;
    use crate::PreparationError;
    use aws_sdk_secretsmanager::operation::get_secret_value::GetSecretValueOutput;
    use aws_sdk_secretsmanager::primitives::Blob;
    use bytes::Bytes;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    const TEST_ARN: &str = "arn:aws:secretsmanager:us-east-1:111122223333:secret:fixture-AbCdEf";
    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone)]
    enum FixtureResponse {
        Decryption,
        Delay(Duration),
        Denied,
        InternalError,
        Json(String),
        Missing,
        Oversize,
        Success,
    }

    fn assert_sdk_fixture_scope() {
        let endpoint =
            std::env::var("AWS_ENDPOINT_URL_SECRETS_MANAGER").expect("fixture endpoint required");
        let endpoint_url = url::Url::parse(&endpoint).expect("fixture URL");
        assert!(
            endpoint_url.scheme() == "http"
                && endpoint_url.host_str() == Some("127.0.0.1")
                && endpoint_url.port().is_some(),
            "SDK child accepts only the literal-loopback fixture"
        );
        assert!(
            std::env::var("AWS_ACCESS_KEY_ID").is_ok_and(|value| value.starts_with("DUMMY")),
            "SDK child requires dummy credentials"
        );
    }

    fn respond(mut stream: TcpStream, response: FixtureResponse) -> (String, String) {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("request read timeout");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).expect("read request headers");
            assert_ne!(read, 0, "SDK closed before request headers");
            request.extend_from_slice(&chunk[..read]);
            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break end
                    .checked_add(4)
                    .expect("header terminator offset fits request");
            }
            assert!(request.len() < 64 * 1024, "bounded fixture request headers");
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:").map(str::trim))
            .and_then(|value| value.parse::<usize>().ok())
            .expect("SDK content length");
        let body_end = header_end
            .checked_add(content_length)
            .expect("bounded request body end fits usize");
        while request.len() < body_end {
            let read = stream.read(&mut chunk).expect("read request body");
            assert_ne!(read, 0, "SDK closed before request body");
            request.extend_from_slice(&chunk[..read]);
        }
        let body = String::from_utf8_lossy(&request[header_end..body_end]).into_owned();
        assert!(headers.contains("x-amz-target: secretsmanager.getsecretvalue"));
        assert!(
            body.contains(TEST_ARN),
            "request must contain only the fictional fixture ARN"
        );
        if let FixtureResponse::Delay(delay) = response {
            thread::sleep(delay);
            let payload = b"{\"SecretString\":\"VALUE_SENTINEL\"}";
            let _header_write = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-amz-json-1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _body_write = stream.write_all(payload);
            return (headers, body);
        }
        let (status, code, payload) = match response {
            FixtureResponse::Decryption => (400_u16, "Bad Request", r#"{"__type":"DecryptionFailure","message":"VALUE_SENTINEL"}"#.to_owned()),
            FixtureResponse::Denied => (400_u16, "Bad Request", r#"{"__type":"AccessDeniedException","message":"ARN_SENTINEL CREDENTIAL_SENTINEL"}"#.to_owned()),
            FixtureResponse::Delay(_) => return (headers, body),
            FixtureResponse::InternalError => (500_u16, "Internal Server Error", r#"{"__type":"InternalServiceError","message":"SDK_ERROR_SENTINEL"}"#.to_owned()),
            FixtureResponse::Json(payload) => (200_u16, "OK", payload),
            FixtureResponse::Missing => (400_u16, "Bad Request", r#"{"__type":"ResourceNotFoundException","message":"ARN_SENTINEL"}"#.to_owned()),
            FixtureResponse::Oversize => (200_u16, "OK", format!(r#"{{"ARN":"{TEST_ARN}","Name":"fixture","VersionId":"12345678901234567890123456789012","SecretString":"{}"}}"#, "x".repeat(0x0001_0001))),
            FixtureResponse::Success => (200_u16, "OK", format!(r#"{{"ARN":"{TEST_ARN}","Name":"fixture","VersionId":"12345678901234567890123456789012","SecretString":"VALUE_SENTINEL"}}"#)),
        };
        let response_body = payload.as_bytes();
        write!(stream, "HTTP/1.1 {status} {code}\r\nContent-Type: application/x-amz-json-1.1\r\nx-amzn-RequestId: fixture-request-id\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response_body.len()).expect("write response headers");
        stream
            .write_all(response_body)
            .expect("write response body");
        (headers, body)
    }

    fn loopback_fixture(
        responses: Vec<FixtureResponse>,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind literal loopback only");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("loopback address");
        let endpoint = format!("http://{address}");
        let worker = thread::spawn(move || {
            let deadline = Instant::now()
                .checked_add(Duration::from_secs(12))
                .expect("fixture deadline fits the monotonic clock");
            let mut bodies = Vec::new();
            for response in responses {
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            assert!(peer.ip().is_loopback(), "fixture peer must be loopback");
                            break stream;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "SDK did not contact fixture");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .expect("response write timeout");
                let (_, body) = respond(stream, response);
                bodies.push(body);
            }
            bodies
        });
        (endpoint, worker)
    }

    fn isolated_sdk_child(endpoint: &str, scenario: &str) -> std::process::Output {
        let directory = std::env::temp_dir().join(format!(
            "edgezero-secrets-fixture-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&directory).expect("create isolated profile directory");
        let config = directory.join("config");
        let credentials = directory.join("credentials");
        std::fs::write(&config, "[profile fixture]\nregion = us-east-1\n")
            .expect("write isolated config");
        std::fs::write(
            &credentials,
            "[fixture]\naws_access_key_id = DUMMY_ACCESS_KEY\naws_secret_access_key = DUMMY_SECRET_KEY\n",
        )
        .expect("write dummy credentials");
        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "secrets_manager::tests::sdk_child_entry",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", &directory)
            .env("AWS_CONFIG_FILE", config)
            .env("AWS_SHARED_CREDENTIALS_FILE", credentials)
            .env("AWS_PROFILE", "fixture")
            .env("AWS_ACCESS_KEY_ID", "DUMMY_ACCESS_KEY")
            .env("AWS_SECRET_ACCESS_KEY", "DUMMY_SECRET_KEY")
            .env("AWS_REGION", "us-east-1")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("AWS_ENDPOINT_URL_SECRETS_MANAGER", endpoint)
            .env("AWS_RETRY_MODE", "standard")
            .env("AWS_MAX_ATTEMPTS", "3")
            .env("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "false")
            .env("EDGEZERO_SECRETS_SCENARIO", scenario)
            .stdin(Stdio::null())
            .output()
            .expect("run isolated SDK child");
        std::fs::remove_dir_all(directory).expect("remove isolated profile directory");
        output
    }

    #[test]
    fn sdk_blob_bytes_are_preserved_without_a_second_base64_decode() {
        let binary = Blob::new(b"YWJjZA==".to_vec());
        assert_eq!(
            materialize_payload(None, Some(&binary)),
            Ok(Bytes::from_static(b"YWJjZA=="))
        );
        assert_eq!(materialize_payload(Some(""), None), Ok(Bytes::new()));
        assert_eq!(
            materialize_payload(None, Some(&Blob::new(Vec::new()))),
            Ok(Bytes::new())
        );
        assert_eq!(
            materialize_payload(None, None),
            Err(PreparationError::Malformed)
        );
        assert_eq!(
            materialize_payload(Some("x"), Some(&binary)),
            Err(PreparationError::Malformed)
        );
    }

    #[test]
    fn real_sdk_uses_child_only_service_endpoint_and_capped_retries() {
        let (endpoint, fixture) = loopback_fixture(vec![
            FixtureResponse::InternalError,
            FixtureResponse::InternalError,
            FixtureResponse::Success,
        ]);
        let child = isolated_sdk_child(&endpoint, "retry");
        let requests = fixture.join().expect("fixture thread");
        assert!(
            child.status.success(),
            "isolated SDK child failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert_eq!(
            requests.len(),
            3,
            "standard SDK performs at most three total attempts"
        );
        assert!(requests.iter().all(|body| body.contains(TEST_ARN)));
    }

    #[test]
    fn real_sdk_serializes_exact_current_stage_and_version_selectors() {
        for (scenario, selector) in [
            ("current", ""),
            ("stage", r#""VersionStage":"BLUE""#),
            (
                "version",
                r#""VersionId":"12345678901234567890123456789012""#,
            ),
        ] {
            let (endpoint, fixture) = loopback_fixture(vec![FixtureResponse::Success]);
            let child = isolated_sdk_child(&endpoint, scenario);
            let requests = fixture.join().expect("fixture thread");
            assert!(
                child.status.success(),
                "isolated SDK child failed for {scenario}: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            assert_eq!(requests.len(), 1);
            assert!(requests[0].contains(TEST_ARN));
            if !selector.is_empty() {
                assert!(
                    requests[0].contains(selector),
                    "selector serialized exactly"
                );
            }
            if scenario == "current" {
                assert!(
                    !requests[0].contains("VersionStage") && !requests[0].contains("VersionId")
                );
            }
        }
    }

    #[test]
    fn real_sdk_service_failures_are_closed_and_non_retryable() {
        for (response, scenario, category) in [
            (FixtureResponse::Denied, "denied", "access denied"),
            (
                FixtureResponse::Decryption,
                "decryption",
                "decryption failure",
            ),
            (
                FixtureResponse::Missing,
                "missing",
                "missing resource or version",
            ),
        ] {
            let (endpoint, fixture) = loopback_fixture(vec![response]);
            let child = isolated_sdk_child(&endpoint, scenario);
            let requests = fixture.join().expect("fixture thread");
            assert!(
                child.status.success(),
                "isolated child rejected expected category: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            assert_eq!(
                requests.len(),
                1,
                "non-retryable service failures use one SDK attempt"
            );
            let output = format!(
                "{}{}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
            assert!(
                output.contains(category),
                "child reports only its closed category"
            );
            for sentinel in [
                "ARN_SENTINEL",
                "VALUE_SENTINEL",
                "CREDENTIAL_SENTINEL",
                "SDK_ERROR_SENTINEL",
            ] {
                assert!(
                    !output.contains(sentinel),
                    "public child output excludes source details"
                );
            }
        }
    }

    #[test]
    fn real_sdk_binary_empty_and_exactly_one_payload_fields() {
        for (scenario, payload) in [
            (
                "binary",
                serde_json::json!({"SecretBinary": "WW1sdVlYSjU="}),
            ),
            ("empty", serde_json::json!({"SecretString": ""})),
            (
                "both",
                serde_json::json!({"SecretString": "opaque", "SecretBinary": ""}),
            ),
            ("neither", serde_json::json!({})),
            (
                "dedup",
                serde_json::json!({"SecretString": "VALUE_SENTINEL"}),
            ),
        ] {
            let (endpoint, fixture) =
                loopback_fixture(vec![FixtureResponse::Json(payload.to_string())]);
            let child = isolated_sdk_child(&endpoint, scenario);
            let requests = fixture.join().expect("fixture thread");
            assert!(
                child.status.success(),
                "payload child failed: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            assert_eq!(
                requests.len(),
                1,
                "exactly one service request despite aliases/repeated snapshot reads"
            );
        }
    }

    #[test]
    fn real_sdk_payload_cap_rejects_materialized_oversize_value() {
        let (endpoint, fixture) = loopback_fixture(vec![FixtureResponse::Oversize]);
        let child = isolated_sdk_child(&endpoint, "oversize");
        let requests = fixture.join().expect("fixture thread");
        assert!(
            child.status.success(),
            "oversize child failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert_eq!(requests.len(), 1);
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(output.contains("size limit"));
        assert!(
            !output.contains(&"x".repeat(128)),
            "payload is not included in public output"
        );
    }

    #[test]
    fn real_sdk_attempt_timeout_is_observed_without_an_outer_retry_loop() {
        let (endpoint, fixture) =
            loopback_fixture(vec![FixtureResponse::Delay(Duration::from_millis(350))]);
        let child = isolated_sdk_child(&endpoint, "timeout");
        let requests = fixture.join().expect("fixture thread");
        assert!(
            child.status.success(),
            "timeout child failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert_eq!(
            requests.len(),
            1,
            "SDK timeout behavior is inside the one provider operation"
        );
    }

    /// Invoked only by an env-cleared child process launched by the parent fixture.
    #[expect(
        clippy::err_expect,
        reason = "the opaque SecretHandle success value intentionally has no Debug API"
    )]
    #[expect(
        clippy::print_stdout,
        reason = "the child reports its closed error category to the parent fixture"
    )]
    #[tokio::test]
    async fn sdk_child_entry() {
        let Ok(scenario) = std::env::var("EDGEZERO_SECRETS_SCENARIO") else {
            return;
        };
        assert_sdk_fixture_scope();
        let version = match scenario.as_str() {
            "stage" => crate::settings::SecretVersion::Stage("BLUE".to_owned()),
            "version" => crate::settings::SecretVersion::VersionId(
                "12345678901234567890123456789012".to_owned(),
            ),
            _ => crate::settings::SecretVersion::Current,
        };
        let target = crate::settings::SecretTarget::new(TEST_ARN.to_owned(), version)
            .expect("valid fictional ARN");
        let settings = crate::settings::SecretsManagerSettings {
            region: "us-east-1".to_owned(),
            entries: std::collections::BTreeMap::from([("active".to_owned(), target)]),
        };
        let limits = if scenario == "timeout" {
            crate::settings::AwsPreparationLimits {
                connect_timeout_ms: 50,
                attempt_timeout_ms: 100,
                operation_timeout_ms: 500,
                preparation_timeout_ms: 2_000,
                ..crate::settings::AwsPreparationLimits::default()
            }
        } else {
            crate::settings::AwsPreparationLimits::default()
        };
        let mut session = crate::preparation::AwsPreparation::new(
            edgezero_core::time::MonotonicClock::default(),
            limits,
        )
        .expect("valid session");
        let result = session.prepare_secrets(&settings, "runtime").await;
        match scenario.as_str() {
            "retry" | "current" | "stage" | "version" | "dedup" => {
                let handle = result.expect("fixture-backed mapped secret prepares");
                let bytes = handle
                    .get_bytes("runtime", "active")
                    .await
                    .expect("snapshot read")
                    .expect("mapped value");
                assert_eq!(bytes.as_ref(), b"VALUE_SENTINEL");
                if scenario == "dedup" {
                    let cached = session
                        .prepare_secrets(&settings, "second")
                        .await
                        .expect("success cache shared across bound namespaces");
                    assert_eq!(
                        cached
                            .get_bytes("second", "active")
                            .await
                            .expect("read")
                            .expect("mapped")
                            .as_ref(),
                        b"VALUE_SENTINEL"
                    );
                    assert_eq!(session.stats().mappings, 2_u64);
                    assert_eq!(session.stats().unique_retained_payload_bytes, 14_u64);
                }
            }
            "binary" | "empty" => {
                let handle = result.expect("payload snapshot");
                let bytes = handle
                    .get_bytes("runtime", "active")
                    .await
                    .expect("snapshot")
                    .expect("mapped empty value is not a miss");
                let expected: &[u8] = if scenario == "binary" {
                    b"YmluYXJ5"
                } else {
                    b""
                };
                assert_eq!(bytes.as_ref(), expected);
            }
            "both" | "neither" => {
                assert_eq!(
                    result.err().expect("exactly one payload field required"),
                    crate::PreparationError::Malformed
                );
            }
            "denied" | "decryption" | "missing" | "timeout" | "oversize" => {
                let expected = match scenario.as_str() {
                    "denied" => crate::PreparationError::AccessDenied,
                    "decryption" => crate::PreparationError::Decryption,
                    "missing" => crate::PreparationError::Missing,
                    "timeout" => crate::PreparationError::Deadline,
                    "oversize" => crate::PreparationError::SizeLimit,
                    _ => panic!("unknown failure scenario"),
                };
                let error = result.err().expect("failed preparation must not publish");
                assert_eq!(error, expected);
                println!("closed-category: {error}");
            }
            _ => panic!("unknown child scenario"),
        }
    }

    #[test]
    fn response_builder_exposes_the_pinned_sdk_fields_used_by_transport() {
        let output = GetSecretValueOutput::builder().secret_string("").build();
        assert_eq!(output.secret_string(), Some(""));
        assert!(output.secret_binary().is_none());
    }
}
