fn main() -> Result<(), fastly::Error> {
    let serve = fixture_fastly::serving().with_max_requests(1);
    fixture_fastly::finish(edgezero_adapter_fastly::serve_app_with_hooks::<
        fixture_fastly::MeasuredApp,
        _,
        _,
        _,
    >(serve, fixture_fastly::extend, |_response| ()))
}
