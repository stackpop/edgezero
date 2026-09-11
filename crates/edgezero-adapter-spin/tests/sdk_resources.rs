#![cfg(all(target_arch = "wasm32", feature = "test-utils"))]

#[cfg(test)]
#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "SDK construction checks stay before the exhaustive classifier fixture"
)]
mod tests {
    use std::time::Duration;

    use edgezero_adapter_spin::outbound::{SpinOutboundClient, map_spin_send_error_for_test};
    use edgezero_core::error::{BadGatewayReason, BudgetSource, EdgeError};
    use edgezero_core::outbound::OutboundHttpClient;
    use edgezero_core::time::Deadline;
    use spin_sdk::wasip3::http::types::{
        DnsErrorPayload, ErrorCode, FieldSizePayload, Fields, Request, RequestOptions, Response,
        TlsAlertReceivedPayload,
    };
    use spin_sdk::wasip3::http_compat::BodyWriter;

    fn assert_outbound_client<Client: OutboundHttpClient>() {}

    #[test]
    fn spin_outbound_client_implements_portable_contract() {
        assert_outbound_client::<SpinOutboundClient>();
    }

    #[test]
    fn sdk_fields_preserve_duplicate_values() {
        let fields = Fields::new();
        fields
            .append("set-cookie", b"first=1")
            .expect("first field append");
        fields
            .append("set-cookie", b"second=2")
            .expect("second field append");

        assert_eq!(
            fields.get("set-cookie"),
            vec![b"first=1".to_vec(), b"second=2".to_vec()]
        );
    }

    #[test]
    fn sdk_request_options_execute_all_setters() {
        let options = RequestOptions::new();
        options
            .set_connect_timeout(Some(1_000_000))
            .expect("connect timeout is supported");
        options
            .set_first_byte_timeout(Some(2_000_000))
            .expect("first-byte timeout is supported");
        options
            .set_between_bytes_timeout(Some(3_000_000))
            .expect("between-bytes timeout is supported");
    }

    #[test]
    fn sdk_request_and_response_completion_resources_construct() {
        let (request_writer, request_body, request_trailers) = BodyWriter::new();
        let (request, request_done) = Request::new(
            Fields::new(),
            Some(request_body),
            request_trailers,
            Some(RequestOptions::new()),
        );

        let (response_writer, response_body, response_trailers) = BodyWriter::new();
        let (response, response_done) =
            Response::new(Fields::new(), Some(response_body), response_trailers);

        drop((request, request_done, request_writer));
        drop((response, response_done, response_writer));
    }

    #[derive(Clone, Copy, Debug)]
    enum ExpectedError {
        BadRequest,
        Internal,
        Protocol,
        Timeout,
        Transport,
        Unknown,
        Unreachable,
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the pinned SDK table intentionally constructs every known ErrorCode variant"
    )]
    fn known_error_codes() -> Vec<(ErrorCode, ExpectedError)> {
        let field = FieldSizePayload {
            field_name: Some("x-test".to_owned()),
            field_size: Some(1),
        };
        vec![
            (ErrorCode::DnsTimeout, ExpectedError::Timeout),
            (
                ErrorCode::DnsError(DnsErrorPayload {
                    rcode: Some("NXDOMAIN".to_owned()),
                    info_code: Some(3),
                }),
                ExpectedError::Unreachable,
            ),
            (ErrorCode::DestinationNotFound, ExpectedError::Unreachable),
            (
                ErrorCode::DestinationUnavailable,
                ExpectedError::Unreachable,
            ),
            (
                ErrorCode::DestinationIpProhibited,
                ExpectedError::Unreachable,
            ),
            (
                ErrorCode::DestinationIpUnroutable,
                ExpectedError::Unreachable,
            ),
            (ErrorCode::ConnectionRefused, ExpectedError::Unreachable),
            (ErrorCode::ConnectionTerminated, ExpectedError::Transport),
            (ErrorCode::ConnectionTimeout, ExpectedError::Timeout),
            (ErrorCode::ConnectionReadTimeout, ExpectedError::Timeout),
            (ErrorCode::ConnectionWriteTimeout, ExpectedError::Timeout),
            (
                ErrorCode::ConnectionLimitReached,
                ExpectedError::Unreachable,
            ),
            (ErrorCode::TlsProtocolError, ExpectedError::Unreachable),
            (ErrorCode::TlsCertificateError, ExpectedError::Unreachable),
            (
                ErrorCode::TlsAlertReceived(TlsAlertReceivedPayload {
                    alert_id: Some(42),
                    alert_message: Some("fixture".to_owned()),
                }),
                ExpectedError::Unreachable,
            ),
            (ErrorCode::HttpRequestDenied, ExpectedError::BadRequest),
            (
                ErrorCode::HttpRequestLengthRequired,
                ExpectedError::Internal,
            ),
            (
                ErrorCode::HttpRequestBodySize(Some(1)),
                ExpectedError::BadRequest,
            ),
            (ErrorCode::HttpRequestMethodInvalid, ExpectedError::Internal),
            (ErrorCode::HttpRequestUriInvalid, ExpectedError::Internal),
            (ErrorCode::HttpRequestUriTooLong, ExpectedError::BadRequest),
            (
                ErrorCode::HttpRequestHeaderSectionSize(Some(1)),
                ExpectedError::BadRequest,
            ),
            (
                ErrorCode::HttpRequestHeaderSize(Some(field.clone())),
                ExpectedError::BadRequest,
            ),
            (
                ErrorCode::HttpRequestTrailerSectionSize(Some(1)),
                ExpectedError::Internal,
            ),
            (
                ErrorCode::HttpRequestTrailerSize(field.clone()),
                ExpectedError::Internal,
            ),
            (ErrorCode::HttpResponseIncomplete, ExpectedError::Protocol),
            (
                ErrorCode::HttpResponseHeaderSectionSize(Some(1)),
                ExpectedError::Protocol,
            ),
            (
                ErrorCode::HttpResponseHeaderSize(field.clone()),
                ExpectedError::Protocol,
            ),
            (
                ErrorCode::HttpResponseBodySize(Some(1)),
                ExpectedError::Protocol,
            ),
            (
                ErrorCode::HttpResponseTrailerSectionSize(Some(1)),
                ExpectedError::Protocol,
            ),
            (
                ErrorCode::HttpResponseTrailerSize(field),
                ExpectedError::Protocol,
            ),
            (
                ErrorCode::HttpResponseTransferCoding(Some("fixture".to_owned())),
                ExpectedError::Protocol,
            ),
            (
                ErrorCode::HttpResponseContentCoding(Some("fixture".to_owned())),
                ExpectedError::Protocol,
            ),
            (ErrorCode::HttpResponseTimeout, ExpectedError::Timeout),
            (ErrorCode::HttpUpgradeFailed, ExpectedError::Protocol),
            (ErrorCode::HttpProtocolError, ExpectedError::Protocol),
            (ErrorCode::LoopDetected, ExpectedError::Protocol),
            (ErrorCode::ConfigurationError, ExpectedError::Internal),
            (
                ErrorCode::InternalError(Some("fixture".to_owned())),
                ExpectedError::Unknown,
            ),
        ]
    }

    fn assert_expected_error(error: &EdgeError, expected: ExpectedError) {
        match expected {
            ExpectedError::BadRequest => assert!(matches!(error, EdgeError::BadRequest { .. })),
            ExpectedError::Internal => assert!(matches!(error, EdgeError::Internal { .. })),
            ExpectedError::Protocol => assert!(matches!(
                error,
                EdgeError::BadGateway {
                    reason: BadGatewayReason::Protocol,
                    ..
                }
            )),
            ExpectedError::Timeout => assert!(matches!(
                error,
                EdgeError::GatewayTimeout {
                    cause: BudgetSource::Unspecified,
                    ..
                }
            )),
            ExpectedError::Transport => assert!(matches!(
                error,
                EdgeError::BadGateway {
                    reason: BadGatewayReason::Transport,
                    ..
                }
            )),
            ExpectedError::Unknown => assert!(matches!(
                error,
                EdgeError::BadGateway {
                    reason: BadGatewayReason::Unspecified,
                    ..
                }
            )),
            ExpectedError::Unreachable => assert!(matches!(
                error,
                EdgeError::BadGateway {
                    reason: BadGatewayReason::Unreachable,
                    ..
                }
            )),
        }
    }

    #[test]
    fn spin_error_code_table_is_exhaustive() {
        let live = Deadline::after(Duration::from_secs(30));
        for (code, expected) in known_error_codes() {
            let mapped = map_spin_send_error_for_test(&code, live, BudgetSource::Default);
            assert_expected_error(&mapped, expected);

            let expired = map_spin_send_error_for_test(
                &code,
                Deadline::after(Duration::ZERO),
                BudgetSource::BatchDeadline,
            );
            assert!(matches!(
                expired,
                EdgeError::GatewayTimeout {
                    cause: BudgetSource::BatchDeadline,
                    ..
                }
            ));
        }
    }

    #[test]
    fn spin_timeout_provenance_before_and_after_deadline() {
        let early = map_spin_send_error_for_test(
            &ErrorCode::DnsTimeout,
            Deadline::after(Duration::from_secs(30)),
            BudgetSource::PerCallTimeout,
        );
        assert!(matches!(
            early,
            EdgeError::GatewayTimeout {
                cause: BudgetSource::Unspecified,
                ..
            }
        ));

        let unreachable = map_spin_send_error_for_test(
            &ErrorCode::ConnectionRefused,
            Deadline::after(Duration::from_secs(30)),
            BudgetSource::Default,
        );
        assert!(matches!(
            unreachable,
            EdgeError::BadGateway {
                reason: BadGatewayReason::Unreachable,
                ..
            }
        ));
    }
}
