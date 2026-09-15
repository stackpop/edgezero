use edgezero_core::app::{App, Hooks, StoresMetadata};
use edgezero_core::router::RouterService;
use std::time::Duration;

struct SlowInitialization;
impl Hooks for SlowInitialization {
    fn routes() -> RouterService {
        fixture_fastly::MeasuredApp::routes()
    }
    fn stores() -> StoresMetadata {
        fixture_fastly::MeasuredApp::stores()
    }
    fn build_app() -> App {
        // A real elapsed deadline, not only the SDK's zero-limit boundary.
        std::thread::sleep(Duration::from_millis(100));
        fixture_fastly::MeasuredApp::build_app()
    }
}
fn main() -> Result<(), fastly::Error> {
    let summary = edgezero_adapter_fastly::serve_app_with_request_extensions::<SlowInitialization, _>(
        fixture_fastly::serving().with_max_lifetime(Duration::from_millis(50)),
        fixture_fastly::extend,
    );
    assert!(summary.time_handler() >= Duration::from_millis(50));
    fixture_fastly::finish(summary)
}
