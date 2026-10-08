fn main() -> anyhow::Result<()> {
    edgezero_adapter_axum::dev_server::run_app::<ActiveApp>()
}

#[cfg(not(feature = "qualification"))]
use fixture_core::FixtureApp as ActiveApp;
#[cfg(feature = "qualification")]
use fixture_core::qualification::QualificationApp as ActiveApp;
