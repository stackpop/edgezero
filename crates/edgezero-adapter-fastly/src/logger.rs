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

/// Initialize logging (opinionated): formatted timestamps using `fern`,
/// chained to the Fastly logger.
///
/// # Errors
/// Returns [`InitLoggerError::Build`] if the underlying logger builder
/// rejects its inputs (e.g. an empty endpoint), or
/// [`InitLoggerError::SetLogger`] if a global logger is already installed.
pub fn init_logger(
    endpoint: &str,
    level: LevelFilter,
    echo_stdout: bool,
) -> Result<(), InitLoggerError> {
    let logger = log_fastly::Logger::builder()
        .default_endpoint(endpoint)
        .echo_stdout(echo_stdout)
        .max_level(level)
        .build()
        .map_err(|err| InitLoggerError::Build(err.to_string()))?;

    // Format timestamps in RFC3339 with milliseconds using UTC to avoid TZ issues in WASM.
    let dispatch = fern::Dispatch::new()
        .level(level)
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} {} {}",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                record.level(),
                message
            ));
        })
        .chain(Box::new(logger) as Box<dyn log::Log>);

    dispatch.apply()?;
    log::set_max_level(level);
    Ok(())
}
