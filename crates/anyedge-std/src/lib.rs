use anyedge_core::Logging;
use chrono::{SecondsFormat, Utc};
use log::LevelFilter;

/// Initialize a simple stdout logger using `fern` with RFC3339-millis timestamps.
pub fn init_logger(level: LevelFilter, _echo_stdout: bool) -> Result<(), log::SetLoggerError> {
    // echo_stdout is a no-op for std logger; logs go to stdout by default.
    let dispatch = fern::Dispatch::new()
        .level(level)
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}  {} {}",
                Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                record.level(),
                message
            ))
        })
        .chain(std::io::stdout());

    dispatch.apply()?;
    log::set_max_level(level);
    Ok(())
}

/// Register the stdout logger initializer with AnyEdge core.
pub fn register_logger(level: LevelFilter, echo_stdout: bool) {
    let init = Box::new(move || init_logger(level, echo_stdout));
    let _ = Logging::set_initializer(init);
}

/// Compatibility helper to match Fastly signature; `endpoint` is ignored.
pub fn register_logger_compat(_endpoint: impl Into<String>, level: LevelFilter, echo_stdout: bool) {
    register_logger(level, echo_stdout);
}
