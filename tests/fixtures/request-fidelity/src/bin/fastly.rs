use edgezero_adapter_fastly::request::{
    capture_request_ingress, dispatch_with_registries_and_ingress,
};
use edgezero_core::app::{Hooks, StoresMetadata};
use edgezero_core::env_config::EnvConfig;
use fastly::{Error, Request, Response};
use request_fidelity::{Fixture, NativeClient};

#[fastly::main]
fn main(mut req: Request) -> Result<Response, Error> {
    let ingress = capture_request_ingress(&req);
    let client_ip = req.get_client_ip_addr();
    // This generic shortcut deliberately demonstrates the SDK normalization gate.
    if req.get_path() == "/native" {
        return Ok(Response::from_body(
            r#"{"native":true,"raw_target_supported":false}"#,
        ));
    }
    // The snapshot must keep the original origin through later native mutation.
    if req.get_header_str("x-fixture") == Some("snapshot-origin") {
        req.set_url("https://spoof.example.com/changed");
        req.set_header("host", "spoof.example.com");
    }
    dispatch_with_registries_and_ingress(
        &Fixture::build_app(),
        req,
        StoresMetadata::default(),
        &EnvConfig::default(),
        ingress,
        |_req, extensions| {
            extensions.insert(NativeClient(client_ip));
        },
    )
}
