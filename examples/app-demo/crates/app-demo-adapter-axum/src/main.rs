use app_demo_core::App;

fn main() {
    if let Err(err) = anyedge_adapter_axum::run_app::<App>(include_str!("../../../anyedge.toml")) {
        eprintln!("app-demo-adapter-axum failed: {err}");
        std::process::exit(1);
    }
}
