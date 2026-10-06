fn main() -> Result<(), fastly::Error> {
    println!(
        "{}",
        serde_json::json!({"event":"heap_preflight", "heap_mib":fastly::compute_runtime::heap_memory_snapshot_mib().ok()})
    );
    // A one-MiB threshold is below this fixture's observed baseline. If the heap
    // snapshot fails, the SDK treats usage as u32::MAX and also stops after one request.
    fixture_fastly::finish(
        edgezero_adapter_fastly::serve_app_with_request_extensions::<fixture_fastly::MeasuredApp, _>(
            fixture_fastly::serving().with_max_memory(1),
            fixture_fastly::extend,
        ),
    )
}
