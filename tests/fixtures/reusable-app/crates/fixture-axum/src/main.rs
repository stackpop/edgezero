fn main() -> anyhow::Result<()> {
    edgezero_adapter_axum::dev_server::run_app::<fixture_core::FixtureApp>()
}
