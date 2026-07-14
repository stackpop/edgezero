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

    #[test]
    fn app_macro_emits_owns_logging_true() {
        assert!(super::OwnedLoggingApp::owns_logging());
    }
}
