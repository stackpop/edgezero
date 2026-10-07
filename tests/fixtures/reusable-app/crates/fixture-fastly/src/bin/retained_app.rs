fn main() -> Result<(), fastly::Error> {
    fixture_fastly::finish(
        edgezero_adapter_fastly::serve_app_with_request_extensions::<fixture_fastly::MeasuredApp, _>(
            fixture_fastly::serving(),
            fixture_fastly::extend,
        ),
    )
}
