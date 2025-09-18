use anyedge_core::App;

#[derive(Debug)]
pub struct CloudflareUnavailable;

pub fn handle(_app: &App, _req: (), _env: (), _ctx: ()) -> () {
    // No-op placeholder; building without `cloudflare` feature.
}
