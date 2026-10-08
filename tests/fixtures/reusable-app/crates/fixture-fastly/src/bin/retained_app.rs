fn main() -> Result<(), fastly::Error> {
    fixture_fastly::finish(edgezero_adapter_fastly::serve_app_with_hooks::<
        fixture_fastly::MeasuredApp,
        _,
        _,
        _,
    >(
        fixture_fastly::serving(),
        fixture_fastly::extend,
        |_response| (),
    ))
}
