#[fastly::main]
fn main(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    edgezero_adapter_fastly::run_app_with_request_extensions::<fixture_fastly::MeasuredApp, _>(
        req,
        fixture_fastly::extend,
    )
}
