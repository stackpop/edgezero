//! Utilities for bridging Fastly Compute@Edge requests into the
//! `edgezero-core` service abstractions.

#![cfg_attr(
    feature = "fastly",
    expect(
        clippy::pub_use,
        reason = "re-export the SDK serving builder rather than duplicate its API"
    )
)]

// Only compiled where it is actually used by the CLI push/GC path. Gating it
// keeps a `--no-default-features` build dead-code clean.
#[cfg(any(feature = "cli", feature = "fastly", test))]
pub(crate) mod chunked_config;
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "fastly")]
pub mod config_store;
pub mod context;
#[cfg(feature = "fastly")]
pub mod key_value_store;
pub mod lifecycle;
#[cfg(feature = "fastly")]
pub mod logger;
#[cfg(feature = "fastly")]
pub mod proxy;
#[cfg(feature = "fastly")]
pub mod request;
#[cfg(feature = "fastly")]
pub mod response;
#[cfg(feature = "fastly")]
pub mod secret_store;

#[cfg(any(feature = "fastly", test))]
use edgezero_core::app::App;
#[cfg(feature = "fastly")]
use edgezero_core::app::Hooks;
#[cfg(feature = "fastly")]
use edgezero_core::http::Extensions;
#[cfg(any(feature = "fastly", test))]
use edgezero_core::manifest::ResolvedLoggingConfig;
#[cfg(feature = "fastly")]
pub use fastly::http::serve::{Serve, ServeSummary};

#[cfg(any(feature = "fastly", test))]
#[derive(Debug, Clone)]
pub struct FastlyLogging {
    pub echo_stdout: bool,
    pub endpoint: Option<String>,
    pub level: log::LevelFilter,
    pub use_fastly_logger: bool,
}

#[cfg(any(feature = "fastly", test))]
impl From<ResolvedLoggingConfig> for FastlyLogging {
    #[inline]
    fn from(config: ResolvedLoggingConfig) -> Self {
        let use_fastly_logger = config.endpoint.is_some();
        Self {
            echo_stdout: config.echo_stdout.unwrap_or(true),
            endpoint: config.endpoint,
            level: config.level.into(),
            use_fastly_logger,
        }
    }
}

#[cfg(any(feature = "fastly", test))]
#[derive(Default)]
struct RetainedApp {
    app: Option<App>,
}

#[cfg(any(feature = "fastly", test))]
impl RetainedApp {
    fn get_or_init<E>(
        &mut self,
        logging: &FastlyLogging,
        owns_logging: impl FnOnce() -> bool,
        install_logger: impl FnOnce(&str, log::LevelFilter, bool) -> Result<(), E>,
        build: impl FnOnce() -> App,
    ) -> Result<&App, E> {
        if self.app.is_none() && logging.use_fastly_logger && !owns_logging() {
            install_logger(
                logging.endpoint.as_deref().unwrap_or("stdout"),
                logging.level,
                logging.echo_stdout,
            )?;
        }
        Ok(self.app.get_or_insert_with(build))
    }
}

/// # Errors
/// Returns [`logger::InitLoggerError::Build`] if the underlying logger
/// builder rejects its inputs (e.g. an empty endpoint), or
/// [`logger::InitLoggerError::SetLogger`] if a global logger is already
/// installed.
#[cfg(feature = "fastly")]
#[inline]
pub fn init_logger(
    endpoint: &str,
    level: log::LevelFilter,
    echo_stdout: bool,
) -> Result<(), logger::InitLoggerError> {
    logger::init_logger(endpoint, level, echo_stdout)
}

/// # Errors
/// Never; this is a no-op stub on builds without the `fastly` feature.
#[cfg(not(feature = "fastly"))]
#[inline]
pub fn init_logger(
    _endpoint: &str,
    _level: log::LevelFilter,
    _echo_stdout: bool,
) -> Result<(), log::SetLoggerError> {
    Ok(())
}

/// Entry point for a Fastly Compute application.
///
/// Portable store declarations and Fastly logging settings are baked into `A`
/// by the `app!` macro. Deployment binds physical stores to the baked logical
/// IDs through Fastly resource links.
///
/// # Errors
/// Logger setup failures and unavailable required stores return errors.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app<A: Hooks>(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    run_app_with_request_extensions::<A, _>(req, |_req, _extensions| {})
}

/// Like [`run_app`], but runs `extend` against a scratch
/// [`Extensions`] populated from the raw
/// `fastly::Request` (TLS JA4, H2 fingerprint, client IP, …) before the request
/// is converted; the scratch values are merged into the core request's
/// extensions and are visible to middleware and the `State`/extractor layer.
///
/// # Errors
/// Logger setup failures and unavailable required stores return errors.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app_with_request_extensions<A, F>(
    req: fastly::Request,
    extend: F,
) -> Result<fastly::Response, fastly::Error>
where
    A: Hooks,
    F: FnOnce(&fastly::Request, &mut Extensions),
{
    let ingress = request::capture_request_ingress(&req);
    let stores = A::stores();
    let logging = FastlyLogging::from(A::logging_for("fastly"));
    if logging.use_fastly_logger && !A::owns_logging() {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
    let app = A::build_app();
    request::dispatch_with_registries_and_ingress(&app, req, stores, ingress, extend)
}

/// Dispatch with a config store wired explicitly. Its name and default key are
/// provided by the caller rather than derived from manifest store metadata.
/// KV is not auto-injected on this path; chain `.with_kv(name)` on a
/// [`request::FastlyService`] builder if you need KV alongside the config store.
///
/// # Errors
/// Returns an error if logger setup fails or the underlying handler returns an error.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app_with_config<A: Hooks>(
    logging: &FastlyLogging,
    req: fastly::Request,
    config_store_name: Option<&str>,
) -> Result<fastly::Response, fastly::Error> {
    if logging.use_fastly_logger && !A::owns_logging() {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
    let app = A::build_app();
    let mut service = request::FastlyService::new(&app);
    if let Some(name) = config_store_name {
        service = service.with_config(name);
    }
    service.dispatch(req)
}

#[cfg(test)]
mod fastly_logging_tests {
    use super::*;
    use edgezero_core::manifest::LogLevel;

    #[test]
    fn fastly_logging_from_manifest_converts_defaults() {
        let config = ResolvedLoggingConfig {
            echo_stdout: Some(false),
            endpoint: Some("endpoint".to_owned()),
            level: LogLevel::Debug,
        };

        let logging: FastlyLogging = config.into();
        assert_eq!(logging.endpoint.as_deref(), Some("endpoint"));
        assert_eq!(logging.level, log::LevelFilter::Debug);
        assert!(!logging.echo_stdout);
        assert!(logging.use_fastly_logger);
    }

    #[test]
    fn fastly_logging_without_manifest_endpoint_does_not_install_named_logger() {
        let logging = FastlyLogging::from(ResolvedLoggingConfig::default());

        assert_eq!(logging.level, log::LevelFilter::Info);
        assert_eq!(logging.endpoint, None);
        assert!(!logging.use_fastly_logger);
        assert!(logging.echo_stdout);
    }
}

#[cfg(test)]
mod retained_app_tests {
    use super::*;
    use edgezero_core::app::{App, Hooks};
    use edgezero_core::router::RouterService;
    use std::cell::{Cell, RefCell};

    struct CountedApp;
    thread_local! { static CONFIGURES: Cell<usize> = const { Cell::new(0) }; }

    #[expect(
        clippy::missing_trait_methods,
        reason = "exercise default app construction"
    )]
    impl Hooks for CountedApp {
        fn configure(app: &mut App) {
            CONFIGURES.with(|count| count.set(count.get().checked_add(1).unwrap()));
            app.set_name("retained");
        }

        fn routes() -> RouterService {
            RouterService::builder().build()
        }
    }

    fn configured_logging() -> FastlyLogging {
        FastlyLogging {
            echo_stdout: true,
            endpoint: Some("fixture-logs".to_owned()),
            level: log::LevelFilter::Debug,
            use_fastly_logger: true,
        }
    }

    #[test]
    fn retained_app_initializes_logging_before_build_once() {
        let mut retained = RetainedApp::default();
        let events = RefCell::new(Vec::new());
        CONFIGURES.with(|count| count.set(0));
        for _ in 0_usize..2 {
            let app = retained
                .get_or_init(
                    &configured_logging(),
                    || false,
                    |endpoint, level, echo| {
                        assert_eq!(endpoint, "fixture-logs");
                        assert_eq!(level, log::LevelFilter::Debug);
                        assert!(echo);
                        events.borrow_mut().push("logger");
                        Ok::<(), &'static str>(())
                    },
                    || {
                        events.borrow_mut().push("build");
                        CountedApp::build_app()
                    },
                )
                .unwrap();
            assert_eq!(app.name(), "retained");
        }
        assert_eq!(*events.borrow(), ["logger", "build"]);
        CONFIGURES.with(|count| assert_eq!(count.get(), 1));
    }

    #[test]
    fn retained_app_skips_disabled_logging_and_builds_once() {
        let mut retained = RetainedApp::default();
        let builds = Cell::new(0_usize);
        let logging = FastlyLogging::from(ResolvedLoggingConfig::default());
        for _ in 0_usize..2 {
            retained
                .get_or_init(
                    &logging,
                    || false,
                    |_, _, _| -> Result<(), &'static str> { panic!("logger is disabled") },
                    || {
                        builds.set(builds.get().checked_add(1).unwrap());
                        CountedApp::build_app()
                    },
                )
                .unwrap();
        }
        assert_eq!(builds.get(), 1);
    }

    #[test]
    fn retained_app_respects_owned_logging_and_fresh_owners() {
        let builds = Cell::new(0_usize);
        for _ in 0_usize..2 {
            let mut retained = RetainedApp::default();
            retained
                .get_or_init(
                    &configured_logging(),
                    || true,
                    |_, _, _| -> Result<(), &'static str> { panic!("caller owns logging") },
                    || {
                        builds.set(builds.get().checked_add(1).unwrap());
                        CountedApp::build_app()
                    },
                )
                .unwrap();
        }
        assert_eq!(builds.get(), 2);
    }

    #[test]
    fn retained_app_logger_error_prevents_construction() {
        let mut retained = RetainedApp::default();
        let result = retained.get_or_init(
            &configured_logging(),
            || false,
            |_, _, _| Err("logger failed"),
            || panic!("must not build"),
        );
        assert_eq!(result.err(), Some("logger failed"));
        assert!(retained.app.is_none());
    }
}

/// Serve requests with an app initialized once on the first callback.
///
/// Opt in using an ordinary `main` and an explicitly bounded [`Serve`]. The
/// runtime may exit before any configured limit; every request must tolerate
/// fresh initialization. The standard response conversion buffers core streams.
/// Inspect the returned summary (whose request count includes failed attempts)
/// or call its `into_result()` method to propagate terminal callback errors.
#[cfg(feature = "fastly")]
#[must_use = "inspect the serving summary or call into_result() to handle terminal errors"]
#[inline]
pub fn serve_app<A: Hooks>(serve: Serve) -> ServeSummary<fastly::Error> {
    serve_app_with_request_extensions::<A, _>(serve, |_request, _extensions| {})
}

/// Serve a retained app with fresh request extensions and store registries.
///
/// Store aliases and logging settings are baked into `A`. The app and logging
/// initialization are retained for the serving loop, while request extensions
/// and store handles are recreated for every callback. Any returned error
/// terminates the SDK loop; handler `EdgeError`s that render as responses do not.
/// Construction panics remain sandbox failures.
///
/// The callback runs per request, but the closure and its captured state live
/// for the entire serving loop. Create request-local mutable data inside the
/// callback. For mutable native requests, manual streaming, response
/// finalization, or custom initialization, use [`lifecycle::serve_custom`].
#[cfg(feature = "fastly")]
#[must_use = "inspect the serving summary or call into_result() to handle terminal errors"]
#[inline]
pub fn serve_app_with_request_extensions<A, F>(
    serve: Serve,
    mut extend: F,
) -> ServeSummary<fastly::Error>
where
    A: Hooks,
    F: FnMut(&fastly::Request, &mut Extensions),
{
    let stores = A::stores();
    let logging = FastlyLogging::from(A::logging_for("fastly"));
    let mut retained = RetainedApp::default();
    serve.run(move |req| -> Result<fastly::Response, fastly::Error> {
        let app = retained.get_or_init(&logging, A::owns_logging, init_logger, A::build_app)?;
        request::dispatch_with_registries(app, req, stores, &mut extend)
    })
}
