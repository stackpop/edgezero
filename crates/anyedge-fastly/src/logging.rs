use log::LevelFilter;

/// Initialize logging (opinionated): formatted timestamps using `fern`,
/// chained to the Fastly logger.
pub fn init_logger(
    endpoint: &str,
    level: LevelFilter,
    echo_stdout: bool,
) -> Result<(), log::SetLoggerError> {
    let logger = log_fastly::Logger::builder()
        .default_endpoint(endpoint)
        .echo_stdout(echo_stdout)
        .max_level(level)
        .build()
        .expect("failed to build Fastly logger");

    // Format timestamps in RFC3339 with milliseconds using UTC to avoid TZ issues in WASM.
    let dispatch = fern::Dispatch::new()
        .level(level)
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}  {} {}",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                record.level(),
                message
            ))
        })
        .chain(Box::new(logger) as Box<dyn log::Log>);

    dispatch.apply()?;
    log::set_max_level(level);
    Ok(())
}
