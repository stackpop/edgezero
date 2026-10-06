fn main() -> Result<(), fastly::Error> {
    fixture_fastly::finish(fixture_fastly::serving().run(|req| {
        let observation = fixture_fastly::observation(&req);
        edgezero_adapter_fastly::run_app_with_request_extensions::<fixture_fastly::MeasuredApp, _>(
            req,
            |_, extensions| {
                extensions.insert(observation);
            },
        )
    }))
}
