use edgezero_core::logging::{BOOT_LOG_LEVEL, BOOT_LOG_TARGET};
use log::LevelFilter;

/// Errors that can occur when initialising the Fastly logger.
#[derive(Debug, thiserror::Error)]
pub enum InitLoggerError {
    /// The `log_fastly::Logger::builder()` rejected its inputs (e.g. the
    /// endpoint string is empty).
    #[error("failed to build Fastly logger: {0}")]
    Build(String),
    /// `log::set_boxed_logger` (via `fern`) failed because a global logger
    /// was already installed.
    #[error(transparent)]
    SetLogger(#[from] log::SetLoggerError),
}

fn build_backend(
    endpoint: &str,
    level: LevelFilter,
    echo_stdout: bool,
) -> Result<log_fastly::Logger, InitLoggerError> {
    log_fastly::Logger::builder()
        .default_endpoint(endpoint)
        .echo_stdout(echo_stdout)
        .max_level(level.max(BOOT_LOG_LEVEL))
        .build()
        .map_err(|err| InitLoggerError::Build(err.to_string()))
}

fn build_dispatch(logger: Box<dyn log::Log>, level: LevelFilter) -> fern::Dispatch {
    fern::Dispatch::new()
        .level(level)
        .level_for(BOOT_LOG_TARGET, BOOT_LOG_LEVEL)
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} {} {}",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                record.level(),
                message
            ));
        })
        .chain(logger)
}

/// Initialize logging (opinionated): formatted timestamps using `fern`,
/// chained to the Fastly logger. Boot warnings are retained independently of
/// the ordinary runtime level, including `Off`.
///
/// # Errors
/// Returns [`InitLoggerError::Build`] if the underlying logger builder
/// rejects its inputs (e.g. an empty endpoint), or
/// [`InitLoggerError::SetLogger`] if a global logger is already installed.
#[inline]
pub fn init_logger(
    endpoint: &str,
    level: LevelFilter,
    echo_stdout: bool,
) -> Result<(), InitLoggerError> {
    let logger = build_backend(endpoint, level, echo_stdout)?;
    build_dispatch(Box::new(logger), level).apply()?;
    Ok(())
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use edgezero_core::logging::{BOOT_LOG_LEVEL, BOOT_LOG_TARGET};
    use log::{Level, LevelFilter, Log as _, Metadata, Record};

    #[test]
    fn backend_rejects_records_even_when_facade_allows_them() {
        let logger = log_fastly::Logger::builder()
            .default_endpoint("logging-test")
            .max_level(LevelFilter::Error)
            .build()
            .expect("SDK logger");
        assert!(
            !logger.enabled(
                &Metadata::builder()
                    .target(BOOT_LOG_TARGET)
                    .level(Level::Warn)
                    .build()
            )
        );
    }

    #[test]
    fn production_filters_preserve_boot_warnings_and_runtime_level() {
        let guard = log_fastly::reset_logger(
            log_fastly::Logger::builder()
                .max_level(LevelFilter::Off)
                .build()
                .expect("isolated SDK capture"),
        );

        for runtime_level in [
            LevelFilter::Off,
            LevelFilter::Error,
            LevelFilter::Warn,
            LevelFilter::Info,
            LevelFilter::Debug,
            LevelFilter::Trace,
        ] {
            let backend =
                super::build_backend("logging-test", runtime_level, false).expect("SDK backend");
            let (ceiling, logger) =
                super::build_dispatch(Box::new(backend), runtime_level).into_log();
            assert_eq!(ceiling, runtime_level.max(BOOT_LOG_LEVEL));
            let preserved_warning = format!("preserved boot warning {runtime_level}");

            for level in [
                Level::Error,
                Level::Warn,
                Level::Info,
                Level::Debug,
                Level::Trace,
            ] {
                let boot = Metadata::builder()
                    .target(BOOT_LOG_TARGET)
                    .level(level)
                    .build();
                let ordinary = Metadata::builder()
                    .target("application")
                    .level(level)
                    .build();
                assert_eq!(logger.enabled(&boot), level <= BOOT_LOG_LEVEL);
                assert_eq!(logger.enabled(&ordinary), level <= runtime_level);
            }

            logger.log(
                &Record::builder()
                    .target(BOOT_LOG_TARGET)
                    .level(Level::Warn)
                    .args(format_args!("{preserved_warning}"))
                    .build(),
            );
            guard.assert_contains("logging-test", &preserved_warning);

            logger.log(
                &Record::builder()
                    .target(BOOT_LOG_TARGET)
                    .level(Level::Info)
                    .args(format_args!("suppressed boot info"))
                    .build(),
            );
            guard.assert_absent("logging-test", "suppressed boot info");

            if runtime_level <= LevelFilter::Error {
                logger.log(
                    &Record::builder()
                        .target("application")
                        .level(Level::Warn)
                        .args(format_args!("suppressed runtime warning"))
                        .build(),
                );
                guard.assert_absent("logging-test", "suppressed runtime warning");
            }
        }
    }
}
