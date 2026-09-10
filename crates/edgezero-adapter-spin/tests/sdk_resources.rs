#![cfg(all(target_arch = "wasm32", feature = "test-utils"))]

#[cfg(test)]
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

    fn known_error_codes() -> Vec<ErrorCode> {
        let field = FieldSizePayload {
            field_name: Some("x-test".to_owned()),
            field_size: Some(1),
        };
        vec![
            ErrorCode::DnsTimeout,
            ErrorCode::DnsError(DnsErrorPayload {
                rcode: Some("NXDOMAIN".to_owned()),
                info_code: Some(3),
            }),
            ErrorCode::DestinationNotFound,
            ErrorCode::DestinationUnavailable,
            ErrorCode::DestinationIpProhibited,
            ErrorCode::DestinationIpUnroutable,
            ErrorCode::ConnectionRefused,
            ErrorCode::ConnectionTerminated,
            ErrorCode::ConnectionTimeout,
            ErrorCode::ConnectionReadTimeout,
            ErrorCode::ConnectionWriteTimeout,
            ErrorCode::ConnectionLimitReached,
            ErrorCode::TlsProtocolError,
            ErrorCode::TlsCertificateError,
            ErrorCode::TlsAlertReceived(TlsAlertReceivedPayload {
                alert_id: Some(42),
                alert_message: Some("fixture".to_owned()),
            }),
            ErrorCode::HttpRequestDenied,
            ErrorCode::HttpRequestLengthRequired,
            ErrorCode::HttpRequestBodySize(Some(1)),
            ErrorCode::HttpRequestMethodInvalid,
            ErrorCode::HttpRequestUriInvalid,
            ErrorCode::HttpRequestUriTooLong,
            ErrorCode::HttpRequestHeaderSectionSize(Some(1)),
            ErrorCode::HttpRequestHeaderSize(Some(field.clone())),
            ErrorCode::HttpRequestTrailerSectionSize(Some(1)),
            ErrorCode::HttpRequestTrailerSize(field.clone()),
            ErrorCode::HttpResponseIncomplete,
            ErrorCode::HttpResponseHeaderSectionSize(Some(1)),
            ErrorCode::HttpResponseHeaderSize(field.clone()),
            ErrorCode::HttpResponseBodySize(Some(1)),
            ErrorCode::HttpResponseTrailerSectionSize(Some(1)),
            ErrorCode::HttpResponseTrailerSize(field),
            ErrorCode::HttpResponseTransferCoding(Some("fixture".to_owned())),
            ErrorCode::HttpResponseContentCoding(Some("fixture".to_owned())),
            ErrorCode::HttpResponseTimeout,
            ErrorCode::HttpUpgradeFailed,
            ErrorCode::HttpProtocolError,
            ErrorCode::LoopDetected,
            ErrorCode::ConfigurationError,
            ErrorCode::InternalError(Some("fixture".to_owned())),
        ]
    }

    #[test]
    fn spin_error_code_table_is_exhaustive() {
        let live = Deadline::after(Duration::from_secs(30));
        for code in known_error_codes() {
            let mapped = map_spin_send_error_for_test(&code, live, BudgetSource::Default);
            assert!(
                matches!(
                    mapped,
                    EdgeError::BadGateway { .. }
                        | EdgeError::BadRequest { .. }
                        | EdgeError::GatewayTimeout { .. }
                        | EdgeError::Internal { .. }
                ),
                "unclassified SDK error: {code:?}"
            );

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
