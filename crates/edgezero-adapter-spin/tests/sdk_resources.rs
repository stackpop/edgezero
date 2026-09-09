#![cfg(target_arch = "wasm32")]

use spin_sdk::wasip3::http::types::{Fields, Request, RequestOptions, Response};
use spin_sdk::wasip3::http_compat::BodyWriter;

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
