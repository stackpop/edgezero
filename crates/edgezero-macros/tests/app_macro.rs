//! Integration coverage: `app!(..., owns_logging = true)` emits a `Hooks` impl
//! whose `owns_logging()` returns `true`. The manifest path resolves against
//! this crate's `CARGO_MANIFEST_DIR`, so the fixture is `tests/fixtures/...`.

// The macro emits `pub struct OwnedLoggingApp;`, a `Hooks` impl, and a free
// `build_router()` at this module scope.
edgezero_core::app!(
    "tests/fixtures/owns_logging.toml",
    OwnedLoggingApp,
    owns_logging = true
);

#[cfg(test)]
mod tests {
    use edgezero_core::app::Hooks as _;
    use edgezero_core::manifest::LogLevel;

    #[test]
    fn app_macro_emits_owns_logging_true() {
        assert!(super::OwnedLoggingApp::owns_logging());
    }

    #[test]
    fn app_macro_bakes_adapter_logging_from_the_manifest() {
        let logging = super::OwnedLoggingApp::logging_for("FASTLY");
        assert_eq!(logging.endpoint.as_deref(), Some("fixture_logs"));
        assert_eq!(logging.level, LogLevel::Debug);
        assert_eq!(logging.echo_stdout, Some(false));

        let missing = super::OwnedLoggingApp::logging_for("custom");
        assert!(missing.endpoint.is_none());
        assert_eq!(missing.level, LogLevel::Info);
        assert!(missing.echo_stdout.is_none());
    }
}
