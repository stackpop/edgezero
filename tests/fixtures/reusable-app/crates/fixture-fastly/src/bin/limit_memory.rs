fn main() -> Result<(), fastly::Error> {
    fastly::init();
    println!(
        "{}",
        serde_json::json!({"event":"heap_preflight", "heap_mib":fastly::compute_runtime::heap_memory_snapshot_mib().ok()})
    );
    // A one-MiB threshold is below this fixture's observed baseline. If the heap
    // snapshot fails, the SDK treats usage as u32::MAX and also stops after one request.
    fixture_fastly::finish(edgezero_adapter_fastly::serve_app_with_hooks::<
        fixture_fastly::MeasuredApp,
        _,
        _,
        _,
    >(
        fixture_fastly::serving().with_max_memory(1),
        fixture_fastly::extend,
        |_response| (),
    ))
}
