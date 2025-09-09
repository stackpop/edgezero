use anyedge_core::App;

#[derive(Debug)]
pub struct WorkersUnavailable;

pub fn handle(_app: &App, _req: (), _env: (), _ctx: ()) -> () {
    // No-op placeholder; building without `workers` feature.
}
