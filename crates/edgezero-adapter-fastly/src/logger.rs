use log::LevelFilter;

/// Initialize logging (opinionated): formatted timestamps using `fern`,
/// chained to the Fastly logger.
pub fn init_logger(
    endpoint: &str,
    level: LevelFilter,
    echo_stdout: bool,
) -> Result<(), log::SetLoggerError> {
    // `.build()` only fails if the endpoint string is empty; callers pass a
    // non-empty endpoint (defaulting to "stdout"). Keeping the panic here
    // preserves the original behavior; widening the error type would be a
    // breaking API change for marginal benefit.
    let logger = log_fastly::Logger::builder()
        .default_endpoint(endpoint)
        .echo_stdout(echo_stdout)
        .max_level(level)
        .build()
        .expect("non-empty Fastly logger endpoint");

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
