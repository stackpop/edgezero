use std::sync::OnceLock;

pub type LoggerInit = Box<dyn Fn() -> Result<(), log::SetLoggerError> + Send + Sync + 'static>;

static LOGGER_INIT: OnceLock<LoggerInit> = OnceLock::new();
static LOGGER_INSTALLED: OnceLock<()> = OnceLock::new();

pub struct Logging;

impl Logging {
    /// Registers a process-wide logging initializer. Returns false if one was already registered.
    pub fn set_initializer(init: LoggerInit) -> bool {
        LOGGER_INIT.set(init).is_ok()
    }

    /// Initialize logging using the previously registered initializer, once per process.
    /// If no initializer is registered, this is a no-op (log macros will be no-ops).
    pub fn init_logging() {
        if let Some(init) = LOGGER_INIT.get() {
            let _ = LOGGER_INSTALLED.get_or_init(|| {
                let _ = (init)();
            });
        }
    }

    /// Convenience: register the provided initializer and then initialize.
    pub fn init_with(init: LoggerInit) {
        let _ = Self::set_initializer(init);
        Self::init_logging();
    }
}
