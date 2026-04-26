use std::process;

use app_demo_core::App;

fn main() {
    if let Err(err) = edgezero_adapter_axum::run_app::<App>(include_str!("../../../edgezero.toml"))
    {
        eprintln!("axum adapter failed: {err}");
        process::exit(1);
    }
}
