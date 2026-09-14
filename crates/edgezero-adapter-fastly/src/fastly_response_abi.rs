#![allow(
    unsafe_code,
    reason = "the status-returning Fastly ABI avoids panic-on-error SDK response wrappers"
)]

use fastly_shared::{
    BodyWriteEnd, FastlyStatus, FramingHeadersMode, INVALID_BODY_HANDLE, INVALID_RESPONSE_HANDLE,
};
use fastly_sys::{BodyHandle, ResponseHandle, fastly_http_body, fastly_http_resp};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FastlyAbiError;

pub(super) fn abandon_body(body: BodyHandle) -> Result<(), FastlyAbiError> {
    // SAFETY: `body` is an owned, live body handle and this call consumes it.
    result(unsafe { fastly_http_body::abandon(body) })
}

pub(super) fn append_header(
    response: ResponseHandle,
    name: &[u8],
    value: &[u8],
) -> Result<(), FastlyAbiError> {
    // SAFETY: both byte slices remain live for the duration of the synchronous hostcall.
    result(unsafe {
        fastly_http_resp::header_append(
            response,
            name.as_ptr(),
            name.len(),
            value.as_ptr(),
            value.len(),
        )
    })
}

pub(super) fn close_body(body: BodyHandle) -> Result<(), FastlyAbiError> {
    // SAFETY: `body` is an owned, live body handle and this call consumes it.
    result(unsafe { fastly_http_body::close(body) })
}

pub(super) fn close_response(response: ResponseHandle) -> Result<(), FastlyAbiError> {
    // SAFETY: `response` is an owned, live response handle and this call consumes it.
    result(unsafe { fastly_http_resp::close(response) })
}

pub(super) fn new_body() -> Result<BodyHandle, FastlyAbiError> {
    let mut handle = INVALID_BODY_HANDLE;
    // SAFETY: the output pointer is valid and points to initialized `u32` storage.
    result(unsafe { fastly_http_body::new(&raw mut handle) })?;
    Ok(handle)
}

pub(super) fn new_response() -> Result<ResponseHandle, FastlyAbiError> {
    let mut handle = INVALID_RESPONSE_HANDLE;
    // SAFETY: the output pointer is valid and points to initialized `u32` storage.
    result(unsafe { fastly_http_resp::new(&raw mut handle) })?;
    Ok(handle)
}

fn result(status: FastlyStatus) -> Result<(), FastlyAbiError> {
    status.result().map_err(|_status| FastlyAbiError)
}

pub(super) fn send_streaming(
    response: ResponseHandle,
    body: BodyHandle,
) -> Result<(), FastlyAbiError> {
    // SAFETY: both handles are exclusively owned and are consumed by the send attempt.
    result(unsafe { fastly_http_resp::send_downstream(response, body, 1) })
}

pub(super) fn set_manual_framing(response: ResponseHandle) -> Result<(), FastlyAbiError> {
    // SAFETY: `response` is a live, exclusively owned response handle.
    result(unsafe {
        fastly_http_resp::framing_headers_mode_set(
            response,
            FramingHeadersMode::ManuallyFromHeaders,
        )
    })
}

pub(super) fn set_status(response: ResponseHandle, status: u16) -> Result<(), FastlyAbiError> {
    // SAFETY: `response` is a live, exclusively owned response handle.
    result(unsafe { fastly_http_resp::status_set(response, status) })
}

pub(super) fn write_body(body: BodyHandle, bytes: &[u8]) -> Result<usize, FastlyAbiError> {
    let mut written = 0;
    // SAFETY: `body` is live, `bytes` remains live during the synchronous hostcall, and the
    // output pointer refers to initialized `usize` storage.
    result(unsafe {
        fastly_http_body::write(
            body,
            bytes.as_ptr(),
            bytes.len(),
            BodyWriteEnd::Back,
            &raw mut written,
        )
    })?;
    Ok(written)
}
