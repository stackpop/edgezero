//! Literal-loopback HTTP tests for startup-only `AppConfig` snapshots.

use std::collections::BTreeMap;
use std::time::Duration;

use edgezero_core::time::MonotonicClock;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::settings::{
    AgentAccessToken, AgentDocument, AppConfigAgentSettings, AwsPreparationLimits,
};
use crate::{AwsPreparation, PreparationError};

async fn fixture(headers: &str, body: Vec<u8>) -> (AppConfigAgentSettings, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("literal loopback");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let response_headers = headers.to_owned();
    let server = tokio::spawn(async move {
        let (mut socket, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("finite accept")
            .expect("accept");
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0_u8; 1024];
            let count = socket.read(&mut chunk).await.expect("request");
            assert_ne!(count, 0, "request did not close before its headers");
            request.extend_from_slice(chunk.get(..count).expect("read buffer bound"));
        }
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n{response_headers}\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("response head");
        let _written = socket.write_all(&body).await;
        String::from_utf8(request).expect("HTTP request")
    });
    let settings = AppConfigAgentSettings {
        endpoint,
        access_token_env: None,
        default_key: "main".to_owned(),
        documents: BTreeMap::from([(
            "main".to_owned(),
            AgentDocument {
                application: "app".to_owned(),
                environment: "test".to_owned(),
                profile: "freeform".to_owned(),
            },
        )]),
    };
    (settings, server)
}

#[tokio::test]
async fn raw_empty_and_quoted_utf8_are_preserved_and_successes_are_deduplicated() {
    for body in ["", "raw free-form\n", "\"quoted\""] {
        let (mut settings, server) = fixture("", body.as_bytes().to_vec()).await;
        settings
            .documents
            .insert("alias".to_owned(), settings.documents["main"].clone());
        let mut session =
            AwsPreparation::new(MonotonicClock::default(), AwsPreparationLimits::default())
                .expect("session");
        let handle = session
            .prepare_appconfig(&settings, None)
            .await
            .expect("snapshot");
        let _request = server.await.expect("server");
        for key in ["main", "alias"] {
            let value = handle
                .get(key)
                .await
                .expect("lookup")
                .expect("mapped value");
            assert_eq!(value.as_ref(), body);
        }
        // The listener is gone. A second store preparation must reuse the successful target.
        let cached = session
            .prepare_appconfig(&settings, None)
            .await
            .expect("cached snapshot");
        assert_eq!(
            cached
                .get("main")
                .await
                .expect("read")
                .expect("value")
                .as_ref(),
            body
        );
        assert_eq!(session.stats().mappings, 4_u64);
        assert_eq!(
            session.stats().unique_retained_payload_bytes,
            u64::try_from(body.len()).expect("length")
        );
        assert!(session.stats().metadata_bytes > 0);
    }
}

#[tokio::test]
async fn bearer_header_and_identity_encoding_are_explicit() {
    let (mut settings, server) = fixture("", b"opaque".to_vec()).await;
    settings.access_token_env = Some("DUMMY_AGENT_TOKEN".to_owned());
    let token = AgentAccessToken::new("dummy-bearer-token".to_owned()).expect("token");
    let mut session =
        AwsPreparation::new(MonotonicClock::default(), AwsPreparationLimits::default())
            .expect("session");
    let _handle = session
        .prepare_appconfig(&settings, Some(&token))
        .await
        .expect("snapshot");
    let request = server.await.expect("server").to_ascii_lowercase();
    assert!(request.contains("authorization: bearer dummy-bearer-token\r\n"));
    assert!(request.contains("accept-encoding: identity\r\n"));
}

#[tokio::test]
async fn nonidentity_encoding_invalid_utf8_and_payload_caps_fail_closed() {
    for (headers, body, cap, expected) in [
        (
            "Content-Encoding: gzip\r\n",
            b"raw".to_vec(),
            4_u64,
            PreparationError::Malformed,
        ),
        (
            "Content-Encoding: identity\r\nContent-Encoding: identity\r\n",
            b"raw".to_vec(),
            4_u64,
            PreparationError::Malformed,
        ),
        (
            "Content-Encoding: identity, identity\r\n",
            b"raw".to_vec(),
            4_u64,
            PreparationError::Malformed,
        ),
        ("", vec![0xff_u8], 4_u64, PreparationError::Malformed),
        ("", b"five!".to_vec(), 4_u64, PreparationError::SizeLimit),
    ] {
        let (settings, server) = fixture(headers, body).await;
        let limits = AwsPreparationLimits {
            max_config_document_bytes: cap,
            ..AwsPreparationLimits::default()
        };
        let mut session = AwsPreparation::new(MonotonicClock::default(), limits).expect("session");
        let error = session
            .prepare_appconfig(&settings, None)
            .await
            .map(|_handle| ())
            .expect_err("closed failure");
        assert_eq!(error, expected);
        assert_eq!(session.stats().unique_retained_payload_bytes, 0);
        let _request = server.await.expect("server");
    }
}
