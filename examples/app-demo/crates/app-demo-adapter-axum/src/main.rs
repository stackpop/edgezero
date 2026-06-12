use app_demo_core::App;
use edgezero_adapter_axum::dev_server::run_app;

fn main() -> anyhow::Result<()> {
    run_app::<App>(include_str!("../../../edgezero.toml"))
}
