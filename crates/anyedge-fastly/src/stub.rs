use anyedge_core::App;

#[derive(Debug)]
pub struct FastlyUnavailable;

pub fn handle(_app: &App, _req: ()) -> () {
    // No-op placeholder; building without `fastly` feature.
}
