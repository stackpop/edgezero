use app_demo_core::App;

fn main() -> anyhow::Result<()> {
    edgezero_adapter_axum::dev_server::run_app::<App>(include_str!("../../../edgezero.toml"))
}
