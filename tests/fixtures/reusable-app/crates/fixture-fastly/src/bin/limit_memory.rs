fn main() -> Result<(), fastly::Error> {
    let serve = {
        let heap = fastly::compute_runtime::heap_memory_snapshot_mib();
        println!(
            "{}",
            serde_json::json!({"event":"heap_preflight", "heap_mib":heap.ok()})
        );
        if heap.is_err() {
            return fixture_fastly::finish(
                edgezero_adapter_fastly::serve_app_with_request_extensions::<
                    fixture_fastly::MeasuredApp,
                    _,
                >(
                    fixture_fastly::serving().with_max_requests(1),
                    fixture_fastly::extend,
                ),
            );
        }
        // A one-MiB threshold is below this fixture's observed baseline. This
        // exercises the supported snapshot/limit branch without unbounded allocation.
        fixture_fastly::serving().with_max_memory(1)
    };
    fixture_fastly::finish(
        edgezero_adapter_fastly::serve_app_with_request_extensions::<fixture_fastly::MeasuredApp, _>(
            serve,
            fixture_fastly::extend,
        ),
    )
}
